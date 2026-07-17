fn main() {
    // The envoy_dynamic_module_callback_* symbols are provided by the host
    // Envoy binary at load time: Linux allows undefined symbols in cdylibs by
    // default and Windows resolves them via the SDK's raw-dylib linking, but
    // macOS needs dynamic lookup requested explicitly.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg=-undefined");
        println!("cargo:rustc-link-arg=dynamic_lookup");
    }
}
