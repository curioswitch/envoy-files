//! Benchmarks envoy-files (compio + blocking) against the built-in
//! file_server, using the same harness as the tests. Drives load with `oha`.
//!
//! Build the module in release, then run the bench (both `--release`, so the
//! module and the load driver are optimized):
//!   cargo build -p envoy-files-filter --release
//!   cargo run -p envoy-files-itest --example bench --release

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use envoy_files_itest::{EnvoyServer, Www, builtin_file_server_config, terminal_config};
use serde_json::{Value, json};

// Per-endpoint measurement window. Overridable (BENCH_DURATION=15s) since large
// files need a longer window for a stable byte-throughput reading.
fn duration() -> String {
    std::env::var("BENCH_DURATION").unwrap_or_else(|_| "10s".to_string())
}

const WARMUP: &str = "2s";
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

fn warmup(port: u16, path: &str, connections: &str) {
    let url = format!("http://127.0.0.1:{port}{path}");
    let _ = Command::new("oha")
        .args(["-z", WARMUP, "-c", connections, "--no-tui", &url])
        .output();
}

fn oha(port: u16, path: &str, connections: &str) -> Sample {
    let url = format!("http://127.0.0.1:{port}{path}");
    let out = Command::new("oha")
        .args([
            "-z",
            &duration(),
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

/// Median time-to-first-byte over a few samples.
fn ttfb_ms(port: u16, path: &str) -> Option<f64> {
    let mut samples: Vec<f64> = (0..7).filter_map(|_| ttfb_once(port, path)).collect();
    if samples.is_empty() {
        return None;
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    Some(samples[samples.len() / 2])
}

fn ttfb_once(port: u16, path: &str) -> Option<f64> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .ok()?;
    let request = format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
    let t0 = Instant::now();
    stream.write_all(request.as_bytes()).ok()?;
    let mut byte = [0u8; 1];
    stream.read_exact(&mut byte).ok()?;
    Some(t0.elapsed().as_secs_f64() * 1000.0)
}

/// Head-of-line blocking probe: measures /index.html latency while /big.bin
/// transfers are continuously in flight. A server that performs file I/O
/// inline on Envoy worker threads stalls every connection sharing the worker,
/// which shows up here as an exploding p99 relative to the plain
/// /index.html line.
fn report_hol(port: u16) {
    let url = format!("http://127.0.0.1:{port}/big.bin");
    // Background big-file traffic, killed once the measurement completes.
    let mut big = match Command::new("oha")
        .args(["-z", "600s", "-c", LARGE_CONN, "--no-tui", &url])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            println!("  hol: skipped (failed to spawn background oha: {error})");
            return;
        }
    };
    // Let the big-file transfers get in flight before measuring.
    std::thread::sleep(Duration::from_secs(1));
    warmup(port, "/index.html", SMALL_CONN);
    let s = oha(port, "/index.html", SMALL_CONN);
    let _ = big.kill();
    let _ = big.wait();
    if s.ok_2xx == 0 {
        println!("  hol: not functional on this platform (no 2xx)");
        return;
    }
    println!(
        "  {:<12} {:>9.1} rps   p50={:>7.2}ms   p99={:>7.2}ms   2xx={}   (+{LARGE_CONN} in-flight /big.bin)",
        "/index+big", s.rps, s.p50_ms, s.p99_ms, s.ok_2xx
    );
}

/// Runs the small- and large-file load against an already-listening port and
/// prints one result line each, plus a head-of-line blocking line. The
/// large-file line also reports TTFB.
fn report(port: u16) {
    for (path, conn, large) in [
        ("/index.html", SMALL_CONN, false),
        ("/big.bin", LARGE_CONN, true),
    ] {
        warmup(port, path, conn);
        let s = oha(port, path, conn);
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
            let secs: f64 = duration().trim_end_matches('s').parse().unwrap_or(10.0);
            let ttfb = ttfb_ms(port, path)
                .map(|ms| format!("{ms:.2}ms"))
                .unwrap_or_else(|| "n/a".to_string());
            println!(
                "   ~{:.0} MiB/s   ttfb={ttfb}",
                s.ok_2xx as f64 * bytes as f64 / secs / (1024.0 * 1024.0)
            );
        } else {
            println!();
        }
    }
    report_hol(port);
}

fn bench(label: &str, config: Value) {
    let server = EnvoyServer::with_config_capturing_backend(config, true);
    let suffix = server
        .io_backend()
        .map(|backend| format!("  [io backend: {backend}]"))
        .unwrap_or_default();
    println!("\n{label}{suffix}");
    report(server.port);
}

/// Best-effort one-shot HTTP GET; returns the numeric status, or None on any
/// transport error (used only to poll for readiness).
fn http_status(port: u16, path: &str) -> Option<u16> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    let mut buf = [0u8; 64];
    let n = stream.read(&mut buf).ok()?;
    // "HTTP/1.1 200 ..." -> parse the status token.
    let head = std::str::from_utf8(&buf[..n]).ok()?;
    head.split(' ').nth(1)?.parse().ok()
}

/// Benchmarks the Built-on-Envoy `file-server` extension via the `boe` CLI,
/// but only when `boe` is installed (CI adds it explicitly). `boe run` manages
/// its own Envoy + the Go composer plugin, so — unlike the other rows, which
/// share our test harness — it runs as a subprocess and we drive load against
/// its listener.
#[cfg(unix)]
fn bench_boe(root: &str) {
    use std::os::unix::process::CommandExt;

    const PORT: u16 = 18080;
    const ADMIN: u16 = 18081;

    println!("\nboe file-server");
    if Command::new("boe")
        .arg("--help")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| !s.success())
        .unwrap_or(true)
    {
        println!("  skipped (boe not installed)");
        return;
    }

    let config = json!({
        "path_mappings": [{"request_path_prefix": "/", "file_path_prefix": root}],
        "content_types": {"html": "text/html", "bin": "application/octet-stream"},
        "default_content_type": "application/octet-stream",
        "directory_index_files": ["index.html"],
    })
    .to_string();

    // `boe run` builds/downloads the extension and Envoy on first use, so allow
    // a generous startup window. Own process group so the whole tree (boe +
    // its Envoy child) can be torn down together.
    let mut child = match Command::new("boe")
        .args([
            "run",
            "--extension",
            "file-server",
            "--dev",
            "--config",
            &config,
            "--listen-port",
            &PORT.to_string(),
            "--admin-port",
            &ADMIN.to_string(),
            "--log-level",
            "all:error",
        ])
        .process_group(0)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            println!("  skipped (failed to spawn boe: {error})");
            return;
        }
    };

    let deadline = Instant::now() + Duration::from_secs(180);
    let mut ready = false;
    while Instant::now() < deadline {
        if matches!(child.try_wait(), Ok(Some(_))) {
            break; // boe exited before serving.
        }
        if http_status(PORT, "/index.html") == Some(200) {
            ready = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }

    if ready {
        report(PORT);
    } else {
        println!("  skipped (boe did not become ready; see `boe logs`)");
    }

    // Tear down the whole process group.
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.wait();
}

#[cfg(not(unix))]
fn bench_boe(_root: &str) {
    println!("\nboe file-server\n  skipped (unix only)");
}

fn main() {
    let www = Www::build();
    let root = www.path().to_str().unwrap();
    println!("root={root}  duration={}  warmup={WARMUP}", duration());

    bench(
        "envoy-files (compio)",
        terminal_config(json!({"root": root})),
    );
    bench(
        "envoy-files (blocking)",
        terminal_config(json!({"root": root, "force_blocking": true})),
    );
    bench("built-in file_server", builtin_file_server_config(root));
    bench_boe(root);
}
