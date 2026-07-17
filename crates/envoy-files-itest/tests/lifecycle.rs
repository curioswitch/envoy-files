use std::io::Read;
use std::sync::LazyLock;

use envoy_files_itest::{EnvoyServer, Www};
use serde_json::json;
use sha2::{Digest, Sha256};

static SERVER: LazyLock<(Www, EnvoyServer)> = LazyLock::new(|| {
    let www = Www::build();
    let server = EnvoyServer::terminal(json!({"root": www.path().to_str().unwrap()}));
    (www, server)
});

#[test]
fn disconnect_midtransfer_then_keep_serving() {
    let server = &SERVER.1;

    // Start several large transfers and abandon them mid-flight.
    for _ in 0..5 {
        let mut stream = server.raw_request("/big.bin");
        let mut chunk = [0u8; 4096];
        let _ = stream.read(&mut chunk);
        drop(stream); // client disconnects mid-response
    }

    // The worker must still be alive and correct.
    let r = server.get("/hello.txt");
    assert_eq!(r.status, 200);
    assert_eq!(r.body, b"hello world\n");

    // And a full large transfer still completes intact.
    let big = server.get("/big.bin");
    assert_eq!(big.status, 200);
    assert_eq!(
        format!("{:x}", Sha256::digest(&big.body)),
        SERVER.0.big_sha256
    );
}

#[test]
fn config_reload_is_clean() {
    // Repeatedly starting and cleanly stopping drops the filter config
    // (engine shutdown + thread join) without hanging or crashing.
    for _ in 0..3 {
        let www = Www::build();
        let server = EnvoyServer::terminal(json!({"root": www.path().to_str().unwrap()}));
        let r = server.get("/hello.txt");
        assert_eq!(r.status, 200);
        assert_eq!(r.body, b"hello world\n");
    }
}
