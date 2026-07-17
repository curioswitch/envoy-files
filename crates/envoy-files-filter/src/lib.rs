// The SDK's set_factory_once! macro compares function pointers.
#![allow(unpredictable_function_pointer_comparisons)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

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
    // A single reactor thread serializes all I/O across Envoy's worker threads,
    // which caps small-file throughput. Run a pool of independent engines (each
    // its own reactor + ring) and hand each stream one by round-robin, so I/O
    // parallelizes across cores like the built-in file_server's thread pool.
    let io_threads = config.io_threads.unwrap_or_else(default_io_threads).max(1);
    let io_config = IoConfig {
        blocking_threads: config
            .blocking_threads
            .unwrap_or_else(|| IoConfig::default().blocking_threads),
        force_blocking: config.force_blocking,
    };
    let mut engines = Vec::with_capacity(io_threads);
    for _ in 0..io_threads {
        match select_backend(&io_config) {
            Ok(engine) => engines.push(engine),
            Err(error) => {
                envoy_log_error!("envoy-files: failed to start io engine: {error}");
                for engine in &engines {
                    engine.shutdown();
                }
                return None;
            }
        }
    }
    envoy_log_info!(
        "envoy-files: serving {:?} with io backend {} ({} io threads)",
        config.root,
        engines[0].backend_name(),
        io_threads,
    );
    Some(Box::new(FilterConfig {
        config: Arc::new(config),
        engines,
        next: AtomicUsize::new(0),
    }))
}

fn default_io_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

struct FilterConfig {
    config: Arc<Config>,
    engines: Vec<Arc<dyn IoEngine>>,
    next: AtomicUsize,
}

impl Drop for FilterConfig {
    fn drop(&mut self) {
        for engine in &self.engines {
            engine.shutdown();
        }
    }
}

impl<EHF: EnvoyHttpFilter> HttpFilterConfig<EHF> for FilterConfig {
    fn new_http_filter(&self, _envoy: &mut EHF) -> Box<dyn HttpFilter<EHF>> {
        // Round-robin a reactor to this stream; it uses that one engine for all
        // of its opens, reads, and the close.
        let idx = self.next.fetch_add(1, Ordering::Relaxed) % self.engines.len();
        Box::new(CatchUnwind::new(Filter::new(
            self.config.clone(),
            self.engines[idx].clone(),
        )))
    }
}
