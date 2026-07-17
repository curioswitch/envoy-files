use std::io::Read;
use std::sync::LazyLock;
use std::time::Duration;

use envoy_files_itest::{EnvoyServer, Www, terminal_config};
use serde_json::json;
use sha2::{Digest, Sha256};

static SERVER: LazyLock<(Www, EnvoyServer)> = LazyLock::new(|| {
    let www = Www::build();
    // Capture Envoy's log (info level) so the module's streaming diagnostics
    // are visible in CI when an assertion fails.
    let server = EnvoyServer::with_config_capturing_backend(
        terminal_config(json!({
            "root": www.path().to_str().unwrap(),
            "chunk_size": 65536,
            "max_inflight_reads": 2,
        })),
        false,
    );
    (www, server)
});

fn dump_module_log(server: &EnvoyServer) {
    if let Some(log) = server.log_contents() {
        for line in log.lines().filter(|l| l.contains("envoy-files")) {
            eprintln!("{line}");
        }
    }
}

fn read_http_body_start(stream: &mut impl Read) -> (Vec<u8>, Vec<u8>) {
    // Read until the header/body separator, returning (headers, leftover body).
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = stream.read(&mut byte).expect("read header byte");
        assert!(n == 1, "connection closed before headers");
        buf.push(byte[0]);
        if buf.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    (buf, Vec::new())
}

#[test]
fn slow_reader_gets_intact_body() {
    let server = &SERVER.1;
    let mut stream = server.raw_request("/big.bin");
    let (headers, _) = read_http_body_start(&mut stream);
    assert!(
        String::from_utf8_lossy(&headers).starts_with("HTTP/1.1 200"),
        "unexpected status line"
    );

    let mut digest = Sha256::new();
    let mut received = 0usize;
    let mut chunk = [0u8; 16384];
    loop {
        // Deliberately slow so Envoy's write buffer fills and the module is
        // forced to pause and resume.
        std::thread::sleep(Duration::from_micros(500));
        let n = stream.read(&mut chunk).expect("read body");
        if n == 0 {
            break;
        }
        digest.update(&chunk[..n]);
        received += n;
    }

    let total = std::fs::metadata(SERVER.0.path().join("big.bin"))
        .unwrap()
        .len() as usize;
    if received != total {
        eprintln!("slow_reader truncated: received={received} total={total}");
        dump_module_log(&SERVER.1);
    }
    assert_eq!(received, total);
    assert_eq!(format!("{:x}", digest.finalize()), SERVER.0.big_sha256);
}

#[test]
fn buffered_bytes_stay_bounded() {
    let server = &SERVER.1;
    let mut stream = server.raw_request("/big.bin");
    let _ = read_http_body_start(&mut stream);
    // Read a little, then stall so buffers fill.
    let mut chunk = [0u8; 4096];
    let _ = stream.read(&mut chunk).expect("initial read");
    std::thread::sleep(Duration::from_secs(1));

    let buffered = server.admin_stat_max("downstream_cx_tx_bytes_buffered");
    drop(stream);

    let file_size = std::fs::metadata(SERVER.0.path().join("big.bin"))
        .unwrap()
        .len();
    // The module's own in-flight window is max_inflight_reads * chunk_size
    // (~128 KiB); even with Envoy's connection buffer, the peak must be far
    // below the whole 100 MiB file.
    assert!(
        buffered < file_size / 4,
        "buffered {buffered} of {file_size}"
    );
}
