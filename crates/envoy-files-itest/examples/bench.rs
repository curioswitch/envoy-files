//! Benchmarks envoy-files (compio + blocking) against the built-in
//! file_server, using the same harness as the tests. Drives load with `oha`.
//!
//! Build the module in release, then run the bench (both `--release`, so the
//! module and the load driver are optimized):
//!   cargo build -p envoy-files-filter --release
//!   cargo run -p envoy-files-itest --example bench --release

use std::process::Command;

use envoy_files_itest::{EnvoyServer, Www, builtin_file_server_config, terminal_config};
use serde_json::{Value, json};

const DURATION: &str = "8s";
const SMALL_CONN: &str = "50";
// Large files at high concurrency just exhaust the loopback accept queue and
// oha reports the refused attempts as failures; low concurrency measures real
// byte throughput.
const LARGE_CONN: &str = "4";

struct Sample {
    rps: f64,
    p50_ms: f64,
    p99_ms: f64,
    ok_2xx: u64,
}

fn oha(port: u16, path: &str, connections: &str) -> Sample {
    let url = format!("http://127.0.0.1:{port}{path}");
    let out = Command::new("oha")
        .args([
            "-z",
            DURATION,
            "-c",
            connections,
            "--no-tui",
            "--output-format",
            "json",
            &url,
        ])
        .output()
        .expect("run oha (is it installed?)");
    let data: Value = serde_json::from_slice(&out.stdout).expect("parse oha json");
    let ok_2xx = data["statusCodeDistribution"]
        .as_object()
        .map(|codes| {
            codes
                .iter()
                .filter(|(k, _)| k.starts_with('2'))
                .filter_map(|(_, v)| v.as_u64())
                .sum()
        })
        .unwrap_or(0);
    Sample {
        rps: data["summary"]["requestsPerSec"].as_f64().unwrap_or(0.0),
        p50_ms: data["latencyPercentiles"]["p50"].as_f64().unwrap_or(0.0) * 1000.0,
        p99_ms: data["latencyPercentiles"]["p99"].as_f64().unwrap_or(0.0) * 1000.0,
        ok_2xx,
    }
}

fn bench(label: &str, config: Value) {
    let server = EnvoyServer::with_config_capturing_backend(config, true);
    let suffix = server
        .io_backend()
        .map(|backend| format!("  [io backend: {backend}]"))
        .unwrap_or_default();
    println!("\n{label}{suffix}");
    for (path, conn, large) in [
        ("/index.html", SMALL_CONN, false),
        ("/big.bin", LARGE_CONN, true),
    ] {
        let s = oha(server.port, path, conn);
        if s.ok_2xx == 0 {
            println!("  {path:<12} not functional on this platform (no 2xx)");
            continue;
        }
        print!(
            "  {path:<12} {:>9.1} rps   p50={:>7.2}ms   p99={:>7.2}ms   2xx={}",
            s.rps, s.p50_ms, s.p99_ms, s.ok_2xx
        );
        if large {
            let bytes = 100 * 1024 * 1024u64;
            let secs: f64 = DURATION.trim_end_matches('s').parse().unwrap_or(8.0);
            println!(
                "   ~{:.0} MiB/s",
                s.ok_2xx as f64 * bytes as f64 / secs / (1024.0 * 1024.0)
            );
        } else {
            println!();
        }
    }
}

fn main() {
    let www = Www::build();
    let root = www.path().to_str().unwrap();
    println!("root={root}  duration={DURATION}");

    bench(
        "envoy-files (compio)",
        terminal_config(json!({"root": root})),
    );
    bench(
        "envoy-files (blocking)",
        terminal_config(json!({"root": root, "force_blocking": true})),
    );
    bench("built-in file_server", builtin_file_server_config(root));
}
