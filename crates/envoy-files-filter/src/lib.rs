// The SDK's set_factory_once! macro compares function pointers.
#![allow(unpredictable_function_pointer_comparisons)]

use std::sync::Arc;

use envoy_files_io::{IoConfig, IoEngine, select_backend};
use envoy_proxy_dynamic_modules_rust_sdk::*;

mod config;
mod eventbridge;
mod filter;

use config::Config;
use filter::Filter;

declare_init_functions!(init, new_http_filter_config_fn);

// Windows has no RTLD_NOLOAD, so the same module may be initialized more than
// once in a process; keep this idempotent.
fn init() -> bool {
    true
}

fn new_http_filter_config_fn<EC: EnvoyHttpFilterConfig, EHF: EnvoyHttpFilter>(
    _envoy_filter_config: &mut EC,
    _filter_name: &str,
    filter_config: &[u8],
) -> Option<Box<dyn HttpFilterConfig<EHF>>> {
    let config = match config::parse(filter_config) {
        Ok(config) => config,
        Err(error) => {
            envoy_log_error!("envoy-files: invalid filter config: {error}");
            return None;
        }
    };
    // A single shared reactor serves all streams. It is I/O-bound (it drives
    // io_uring/IOCP, not CPU-heavy work), so one keeps up; adding reactors only
    // oversubscribes against Envoy's own per-core worker threads and lowers
    // throughput (measured: rps fell monotonically as reactor count rose).
    let engine = match select_backend(&IoConfig {
        blocking_threads: config
            .blocking_threads
            .unwrap_or_else(|| IoConfig::default().blocking_threads),
        force_blocking: config.force_blocking,
    }) {
        Ok(engine) => engine,
        Err(error) => {
            envoy_log_error!("envoy-files: failed to start io engine: {error}");
            return None;
        }
    };
    envoy_log_info!(
        "envoy-files: serving {:?} with io backend {}",
        config.root,
        engine.backend_name(),
    );
    Some(Box::new(FilterConfig {
        config: Arc::new(config),
        engine,
    }))
}

struct FilterConfig {
    config: Arc<Config>,
    engine: Arc<dyn IoEngine>,
}

impl Drop for FilterConfig {
    fn drop(&mut self) {
        self.engine.shutdown();
    }
}

impl<EHF: EnvoyHttpFilter> HttpFilterConfig<EHF> for FilterConfig {
    fn new_http_filter(&self, _envoy: &mut EHF) -> Box<dyn HttpFilter<EHF>> {
        Box::new(CatchUnwind::new(Filter::new(
            self.config.clone(),
            self.engine.clone(),
        )))
    }
}
