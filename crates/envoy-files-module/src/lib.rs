// The SDK's set_factory_once! macro compares function pointers.
#![allow(unpredictable_function_pointer_comparisons)]

use envoy_proxy_dynamic_modules_rust_sdk::*;

declare_init_functions!(init, envoy_files_filter::new_http_filter_config_fn);

// Windows has no RTLD_NOLOAD, so the same module may be initialized more than
// once in a process; keep this idempotent.
fn init() -> bool {
    true
}
