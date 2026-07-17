//! Test harness that drives a real Envoy (fetched from the `envoy-server`
//! wheel) loading the `envoy-files` dynamic module. Pure Rust; no Python.

mod envoy;
mod fixtures;

pub use fixtures::Www;

use std::io::Write;
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

pub const MODULE_NAME: &str = "envoy_files";

/// A running Envoy instance serving through the module.
pub struct EnvoyServer {
    process: Option<Child>,
    module_dir: PathBuf,
    force_kill: bool,
    /// Set when started with backend capture; the file Envoy's stderr is
    /// written to, holding the module's startup log line.
    log_path: Option<PathBuf>,
    pub admin_address: String,
    pub port: u16,
}

pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

impl EnvoyServer {
    /// Starts Envoy with the module as a terminal filter serving `filter_cfg`.
    pub fn terminal(filter_cfg: Value) -> EnvoyServer {
        Self::start(terminal_config(filter_cfg), false, false)
    }

    /// Starts Envoy with a fully specified bootstrap config.
    pub fn with_config(config: Value, force_kill: bool) -> EnvoyServer {
        Self::start(config, force_kill, false)
    }

    /// Like [`with_config`](Self::with_config), but captures Envoy's log so
    /// [`io_backend`](Self::io_backend) can report the module's chosen driver.
    pub fn with_config_capturing_backend(config: Value, force_kill: bool) -> EnvoyServer {
        Self::start(config, force_kill, true)
    }

    fn start(config: Value, force_kill: bool, capture_log: bool) -> EnvoyServer {
        let module_dir = unique_tmp("module");
        std::fs::create_dir_all(&module_dir).expect("create module dir");
        // Envoy resolves `name` to lib<name>.so on POSIX (incl. macOS) and
        // <name>.dll on Windows.
        let staged = module_dir.join(if cfg!(windows) {
            format!("{MODULE_NAME}.dll")
        } else {
            format!("lib{MODULE_NAME}.so")
        });
        std::fs::copy(envoy::module_path(), &staged).expect("stage module");

        // To capture the module's startup line ("... with io backend <name>")
        // we raise the level to info and send stderr to a file (a file, not a
        // pipe, so a static-held server still can't hang on EOF). Envoy logs
        // nothing per-request at info, so captured runs stay representative.
        let (log_level, log_path, stderr) = if capture_log {
            let path = module_dir.join("envoy.log");
            let file = std::fs::File::create(&path).expect("create envoy log");
            ("info", Some(path), Stdio::from(file))
        } else {
            ("error", None, Stdio::null())
        };

        let admin_file = unique_tmp("admin").with_extension("txt");
        let process = Command::new(envoy::envoy_binary())
            .args([
                "--config-yaml",
                &config.to_string(),
                "--admin-address-path",
                admin_file.to_str().unwrap(),
                // Hot restart prevents using a static Envoy among tests since threads dying
                // get propagated to Envoy itself when hot restart is enabled.
                "--disable-hot-restart",
                "--log-level",
                log_level,
            ])
            .env("ENVOY_DYNAMIC_MODULES_SEARCH_PATH", &module_dir)
            // Detach stdio: a server held in a `static` never runs Drop, and
            // an inherited stdout pipe would keep `cargo test | ...` from ever
            // seeing EOF. The atexit hook below still kills it.
            .stdout(Stdio::null())
            .stderr(stderr)
            .spawn()
            .expect("spawn envoy");
        register_for_cleanup(process.id());

        let mut server = EnvoyServer {
            process: Some(process),
            module_dir,
            force_kill,
            log_path,
            admin_address: String::new(),
            port: 0,
        };
        server.await_ready(&admin_file);
        let _ = std::fs::remove_file(&admin_file);
        server
    }

    /// The io backend the module logged at startup ("compio-io_uring",
    /// "compio-poll", "compio-iocp", ...). Only populated when started with
    /// [`with_config_capturing_backend`](Self::with_config_capturing_backend);
    /// `None` for a config that doesn't load the module (e.g. the built-in
    /// file_server). Polls briefly, since Envoy may buffer the log line.
    pub fn io_backend(&self) -> Option<String> {
        let path = self.log_path.as_ref()?;
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Ok(log) = std::fs::read_to_string(path)
                && let Some(rest) = log.split("with io backend ").nth(1)
                && let Some(name) = rest.split_whitespace().next()
            {
                return Some(name.to_string());
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn await_ready(&mut self, admin_file: &std::path::Path) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if let Some(process) = &mut self.process
                && let Ok(Some(status)) = process.try_wait()
            {
                panic!("envoy exited during startup: {status}");
            }
            if let Ok(admin) = std::fs::read_to_string(admin_file)
                && !admin.trim().is_empty()
            {
                let admin = admin.trim().to_string();
                if let Some(port) = discover_port(&admin) {
                    self.admin_address = admin;
                    self.port = port;
                    return;
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("timed out waiting for envoy to start");
    }

    pub fn get(&self, path: &str) -> Response {
        self.request("GET", path, &[])
    }

    pub fn request(&self, method: &str, path: &str, headers: &[(&str, &str)]) -> Response {
        // Retry only on transport errors (connection reset / short read),
        // which happen occasionally on loopback when many threads hammer the
        // shared server while one streams a large body. This never hides a
        // product bug: status/header/body assertions run on the returned
        // Response, and a genuine fault fails identically on every attempt.
        let mut last_err = None;
        for _ in 0..3 {
            match self.try_request(method, path, headers) {
                Ok(response) => return response,
                Err(err) => last_err = Some(err),
            }
        }
        panic!("http request to {path} failed after retries: {last_err:?}");
    }

    fn try_request(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
    ) -> Result<Response, ureq::Error> {
        let url = format!("http://127.0.0.1:{}{path}", self.port);
        let mut builder = ureq::http::Request::builder().method(method).uri(url);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let request = builder.body(()).expect("build request");
        let response = agent().run(request)?;
        let status = response.status().as_u16();
        let header_pairs = response
            .headers()
            .iter()
            .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();
        // Lift ureq's default 10 MiB body cap for the large-file test.
        let body = response
            .into_body()
            .into_with_config()
            .limit(u64::MAX)
            .read_to_vec()?;
        Ok(Response {
            status,
            headers: header_pairs,
            body,
        })
    }

    /// Raw HTTP/1.1 request over a bare socket, returning the connection so a
    /// test can read the body slowly (backpressure / disconnect scenarios).
    /// Deliberately keep-alive: `Connection: close` would put Envoy into its
    /// deferred-close path when the response completes, which can race a
    /// slow-draining client; callers frame the body by `Content-Length`.
    pub fn raw_request(&self, path: &str) -> TcpStream {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).expect("connect to envoy");
        let request = format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n");
        stream.write_all(request.as_bytes()).expect("write request");
        stream
    }

    /// Value of an exactly-named counter/gauge (0 if absent).
    pub fn admin_stat(&self, name: &str) -> u64 {
        self.stats(name)
            .into_iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v)
            .unwrap_or(0)
    }

    /// Max value across all counters/gauges whose name contains `substr`.
    /// Robust to stat names that vary across Envoy versions.
    pub fn admin_stat_max(&self, substr: &str) -> u64 {
        self.stats(substr)
            .into_iter()
            .map(|(_, v)| v)
            .max()
            .unwrap_or(0)
    }

    fn stats(&self, filter: &str) -> Vec<(String, u64)> {
        let url = format!("http://{}/stats?filter={filter}", self.admin_address);
        let body = agent()
            .get(&url)
            .call()
            .expect("admin stats")
            .body_mut()
            .read_to_string()
            .expect("read stats");
        body.lines()
            .filter_map(|line| {
                let (key, value) = line.split_once(": ")?;
                Some((key.to_string(), value.trim().parse().ok()?))
            })
            .collect()
    }
}

impl Drop for EnvoyServer {
    fn drop(&mut self) {
        if let Some(mut process) = self.process.take() {
            if self.force_kill {
                let _ = process.kill();
            } else {
                #[cfg(unix)]
                {
                    // SIGTERM for a graceful stop.
                    unsafe {
                        libc_kill(process.id() as i32);
                    }
                }
                #[cfg(not(unix))]
                {
                    let _ = process.kill();
                }
            }
            let _ = process.wait();
        }
        let _ = std::fs::remove_dir_all(&self.module_dir);
    }
}

#[cfg(unix)]
unsafe fn libc_kill(pid: i32) {
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }
}

// Servers held in a `static` (module-scoped fixtures) never run Drop, so their
// Envoy child would leak. Track every spawned pid and SIGKILL survivors at
// normal process exit.
static SPAWNED_PIDS: Mutex<Vec<u32>> = Mutex::new(Vec::new());

fn register_for_cleanup(pid: u32) {
    SPAWNED_PIDS.lock().unwrap().push(pid);
    #[cfg(unix)]
    {
        static REGISTER: std::sync::Once = std::sync::Once::new();
        REGISTER.call_once(|| unsafe {
            libc::atexit(kill_spawned);
        });
    }
}

#[cfg(unix)]
extern "C" fn kill_spawned() {
    if let Ok(pids) = SPAWNED_PIDS.lock() {
        for &pid in pids.iter() {
            unsafe {
                libc::kill(pid as i32, libc::SIGKILL);
            }
        }
    }
}

fn agent() -> ureq::Agent {
    // Surface all status codes as responses (not errors) and never follow
    // redirects, so tests can assert 301/404/416 directly.
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .build()
        .into()
}

fn discover_port(admin: &str) -> Option<u16> {
    let url = format!("http://{admin}/listeners?format=json");
    let body = agent()
        .get(&url)
        .call()
        .ok()?
        .body_mut()
        .read_to_string()
        .ok()?;
    let json: Value = serde_json::from_str(&body).ok()?;
    let socket = &json["listener_statuses"][0]["local_address"]["socket_address"];
    socket["port_value"].as_u64().map(|p| p as u16)
}

fn unique_tmp(kind: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "envoy-files-itest-{kind}-{}-{n}",
        std::process::id()
    ))
}

/// Builds a dynamic-module HTTP filter entry.
pub fn dynamic_module_filter(filter_cfg: &Value, terminal: bool) -> Value {
    json!({
        "name": MODULE_NAME,
        "typed_config": {
            "@type": "type.googleapis.com/envoy.extensions.filters.http.dynamic_modules.v3.DynamicModuleFilter",
            "dynamic_module_config": {"name": MODULE_NAME},
            "filter_name": MODULE_NAME,
            "terminal_filter": terminal,
            "filter_config": {
                "@type": "type.googleapis.com/google.protobuf.StringValue",
                "value": filter_cfg.to_string(),
            },
        },
    })
}

const HCM: &str = "type.googleapis.com/envoy.extensions.filters.network.http_connection_manager.v3.HttpConnectionManager";

/// Minimal bootstrap: the module as the single, terminal HTTP filter.
pub fn terminal_config(filter_cfg: Value) -> Value {
    json!({
        "admin": {"address": {"socket_address": {"address": "127.0.0.1", "port_value": 0}}},
        "static_resources": {
            "listeners": [{
                "name": "main",
                "address": {"socket_address": {"address": "127.0.0.1", "port_value": 0}},
                "filter_chains": [{
                    "filters": [{
                        "name": "envoy.filters.network.http_connection_manager",
                        "typed_config": {
                            "@type": HCM,
                            "stat_prefix": "ingress_http",
                            "generate_request_id": false,
                            "route_config": {"virtual_hosts": [{"name": "local", "domains": ["*"]}]},
                            "http_filters": [dynamic_module_filter(&filter_cfg, true)],
                        },
                    }],
                }],
            }],
        },
    })
}

/// Bootstrap that mounts the module at multiple path prefixes with Envoy's
/// matching API (`ExtensionWithMatcher` + `composite`) — the pattern for
/// serving static folders alongside other backends. Each `(prefix, cfg)` runs
/// the module (terminal) for requests under `prefix`; unmatched paths fall
/// through to the router and get a 404.
pub fn mounts_config(mounts: &[(&str, Value)]) -> Value {
    let mut map = serde_json::Map::new();
    for (prefix, filter_cfg) in mounts {
        map.insert(
            prefix.to_string(),
            json!({
                "action": {
                    "name": "composite-action",
                    "typed_config": {
                        "@type": "type.googleapis.com/envoy.extensions.filters.http.composite.v3.ExecuteFilterAction",
                        "typed_config": dynamic_module_filter(filter_cfg, true),
                    },
                },
            }),
        );
    }
    json!({
        "admin": {"address": {"socket_address": {"address": "127.0.0.1", "port_value": 0}}},
        "static_resources": {"listeners": [{
            "name": "main",
            "address": {"socket_address": {"address": "127.0.0.1", "port_value": 0}},
            "filter_chains": [{"filters": [{
                "name": "envoy.filters.network.http_connection_manager",
                "typed_config": {
                    "@type": HCM,
                    "stat_prefix": "ingress_http",
                    "generate_request_id": false,
                    "route_config": {"virtual_hosts": [{
                        "name": "local", "domains": ["*"],
                        "routes": [{"match": {"prefix": "/"}, "direct_response": {"status": 404, "body": {"inline_string": "not found"}}}],
                    }]},
                    "http_filters": [
                        {
                            "name": "envoy.filters.http.composite",
                            "typed_config": {
                                "@type": "type.googleapis.com/envoy.extensions.common.matching.v3.ExtensionWithMatcher",
                                "extension_config": {
                                    "name": "envoy.filters.http.composite",
                                    "typed_config": {"@type": "type.googleapis.com/envoy.extensions.filters.http.composite.v3.Composite"},
                                },
                                "xds_matcher": {"matcher_tree": {
                                    "input": {
                                        "name": "request-path",
                                        "typed_config": {
                                            "@type": "type.googleapis.com/envoy.type.matcher.v3.HttpRequestHeaderMatchInput",
                                            "header_name": ":path",
                                        },
                                    },
                                    "prefix_match_map": {"map": Value::Object(map)},
                                }},
                            },
                        },
                        {"name": "envoy.filters.http.router", "typed_config": {"@type": "type.googleapis.com/envoy.extensions.filters.http.router.v3.Router"}},
                    ],
                },
            }]}],
        }]},
    })
}

/// Bootstrap using Envoy's built-in `file_server` filter over `root`, for
/// benchmark comparison. Not functional on macOS/Windows (POSIX-coupled).
pub fn builtin_file_server_config(root: &str) -> Value {
    json!({
        "admin": {"address": {"socket_address": {"address": "127.0.0.1", "port_value": 0}}},
        "static_resources": {"listeners": [{
            "name": "main",
            "address": {"socket_address": {"address": "127.0.0.1", "port_value": 0}},
            "filter_chains": [{"filters": [{
                "name": "envoy.filters.network.http_connection_manager",
                "typed_config": {
                    "@type": HCM,
                    "stat_prefix": "ingress_http",
                    "generate_request_id": false,
                    "route_config": {"virtual_hosts": [{"name": "local", "domains": ["*"]}]},
                    "http_filters": [
                        {
                            "name": "envoy.filters.http.file_server",
                            "typed_config": {
                                "@type": "type.googleapis.com/envoy.extensions.filters.http.file_server.v3.FileServerConfig",
                                "manager_config": {"thread_pool": {"thread_count": 4}},
                                "path_mappings": [{"request_path_prefix": "/", "file_path_prefix": root}],
                                "content_types": {"html": "text/html", "bin": "application/octet-stream"},
                                "default_content_type": "application/octet-stream",
                            },
                        },
                        {"name": "envoy.filters.http.router", "typed_config": {"@type": "type.googleapis.com/envoy.extensions.filters.http.router.v3.Router"}},
                    ],
                },
            }]}],
        }]},
    })
}

/// CacheV2 in the outer listener, routing into an internal listener that
/// terminates with the module. Proves composition without a fake upstream.
pub fn cachev2_composition_config(filter_cfg: Value) -> Value {
    json!({
        "admin": {"address": {"socket_address": {"address": "127.0.0.1", "port_value": 0}}},
        "bootstrap_extensions": [{
            "name": "envoy.bootstrap.internal_listener",
            "typed_config": {"@type": "type.googleapis.com/envoy.extensions.bootstrap.internal_listener.v3.InternalListener"},
        }],
        "static_resources": {
            "listeners": [
                {
                    "name": "main",
                    "address": {"socket_address": {"address": "127.0.0.1", "port_value": 0}},
                    "filter_chains": [{"filters": [{
                        "name": "envoy.filters.network.http_connection_manager",
                        "typed_config": {
                            "@type": HCM,
                            "stat_prefix": "outer",
                            "generate_request_id": false,
                            "route_config": {"virtual_hosts": [{
                                "name": "local", "domains": ["*"],
                                "routes": [{"match": {"prefix": "/"}, "route": {"cluster": "files_internal"}}],
                            }]},
                            "http_filters": [
                                {
                                    "name": "envoy.filters.http.cache",
                                    "typed_config": {
                                        "@type": "type.googleapis.com/envoy.extensions.filters.http.cache_v2.v3.CacheV2Config",
                                        "typed_config": {"@type": "type.googleapis.com/envoy.extensions.http.cache_v2.simple_http_cache.v3.SimpleHttpCacheV2Config"},
                                    },
                                },
                                {"name": "envoy.filters.http.router", "typed_config": {"@type": "type.googleapis.com/envoy.extensions.filters.http.router.v3.Router"}},
                            ],
                        },
                    }]}],
                },
                {
                    "name": "files_internal_listener",
                    "internal_listener": {},
                    "filter_chains": [{"filters": [{
                        "name": "envoy.filters.network.http_connection_manager",
                        "typed_config": {
                            "@type": HCM,
                            "stat_prefix": "inner",
                            "generate_request_id": false,
                            "route_config": {"virtual_hosts": [{"name": "inner", "domains": ["*"]}]},
                            "http_filters": [dynamic_module_filter(&filter_cfg, true)],
                        },
                    }]}],
                },
            ],
            "clusters": [{
                "name": "files_internal",
                "connect_timeout": "1s",
                "load_assignment": {
                    "cluster_name": "files_internal",
                    "endpoints": [{"lb_endpoints": [{"endpoint": {"address": {
                        "envoy_internal_address": {"server_listener_name": "files_internal_listener"}
                    }}}]}],
                },
            }],
        },
    })
}
