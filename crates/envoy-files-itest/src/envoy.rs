//! Locates the Envoy binary (fetched from the `envoy-server` PyPI wheel and
//! cached under `target/`) and the freshly built module `cdylib`.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

const ENVOY_SERVER_VERSION: &str = "1.38.3";

/// Path to the Envoy binary, fetched and cached on first use.
pub fn envoy_binary() -> &'static Path {
    static BINARY: LazyLock<PathBuf> = LazyLock::new(fetch_envoy);
    &BINARY
}

/// Path to the module `cdylib`. The module must be built beforehand — the
/// harness only locates it (it never runs `cargo` inside a test). The profile
/// matches this test binary's own, so `cargo test` finds the debug build and
/// `cargo run --release` (the bench) finds the release build.
pub fn module_path() -> &'static Path {
    static MODULE: LazyLock<PathBuf> = LazyLock::new(locate_module);
    &MODULE
}

fn target_dir() -> PathBuf {
    // CARGO_TARGET_TMPDIR is <target>/tmp/<pkg>; walk up to <target>.
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(Path::parent)
        .map(|root| root.join("target"))
        .expect("workspace target dir")
}

fn fetch_envoy() -> PathBuf {
    let cache = target_dir().join("envoy-bin");
    let binary = cache.join(if cfg!(windows) { "envoy.dll" } else { "envoy" });
    if binary.exists() {
        return binary;
    }
    std::fs::create_dir_all(&cache).expect("create envoy cache dir");

    let wheel_url = wheel_url();
    eprintln!("itest: downloading Envoy wheel {wheel_url}");
    let mut wheel = Vec::new();
    ureq::get(&wheel_url)
        .call()
        .expect("download envoy wheel")
        .into_body()
        .into_reader()
        .read_to_end(&mut wheel)
        .expect("read envoy wheel body");

    let entry_name = if cfg!(windows) {
        "envoy/_bin/envoy.dll"
    } else {
        "envoy/_bin/envoy"
    };
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(wheel)).expect("open wheel zip");
    let mut entry = archive
        .by_name(entry_name)
        .unwrap_or_else(|_| panic!("wheel missing {entry_name}"));
    let mut bytes = Vec::with_capacity(entry.size() as usize);
    entry.read_to_end(&mut bytes).expect("read binary entry");
    drop(entry);

    let tmp = cache.join(".envoy.partial");
    std::fs::write(&tmp, &bytes).expect("write envoy binary");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
            .expect("chmod envoy");
    }
    std::fs::rename(&tmp, &binary).expect("finalize envoy binary");
    binary
}

fn wheel_url() -> String {
    let platform_tag = platform_tag();
    let index = format!("https://pypi.org/pypi/envoy-server/{ENVOY_SERVER_VERSION}/json");
    let body = ureq::get(&index)
        .call()
        .expect("query pypi")
        .into_body()
        .read_to_string()
        .expect("read pypi json");
    let meta: serde_json::Value = serde_json::from_str(&body).expect("parse pypi json");
    let urls = meta["urls"].as_array().expect("pypi urls array");
    for url in urls {
        let filename = url["filename"].as_str().unwrap_or("");
        if filename.ends_with(".whl") && filename.contains(&platform_tag) {
            return url["url"].as_str().expect("wheel url").to_string();
        }
    }
    panic!("no envoy-server wheel for platform tag {platform_tag}");
}

/// The substring of the wheel filename identifying this platform.
fn platform_tag() -> String {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "macosx".to_string(),
        ("linux", "aarch64") => "manylinux_2_31_aarch64".to_string(),
        ("linux", "x86_64") => "manylinux_2_31_x86_64".to_string(),
        ("windows", "x86_64") => "win_amd64".to_string(),
        (os, arch) => panic!("unsupported platform for envoy-server: {os}/{arch}"),
    }
}

fn locate_module() -> PathBuf {
    let (name, ext) = if cfg!(windows) {
        ("envoy_files", "dll")
    } else if cfg!(target_os = "macos") {
        ("libenvoy_files", "dylib")
    } else {
        ("libenvoy_files", "so")
    };
    // Match this binary's profile: debug for `cargo test`, release for the
    // `--release` bench.
    let (profile_dir, release_flag) = if cfg!(debug_assertions) {
        ("debug", "")
    } else {
        ("release", " --release")
    };
    let path = target_dir().join(profile_dir).join(format!("{name}.{ext}"));
    assert!(
        path.exists(),
        "module not found at {} — build it first: \
         `cargo build --package envoy-files-filter{release_flag}`",
        path.display(),
    );
    path
}
