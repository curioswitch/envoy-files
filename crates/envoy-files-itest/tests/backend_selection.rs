use envoy_files_itest::{EnvoyServer, Www};
use serde_json::json;

// The compio backend is the default and is covered by every other test; this
// exercises the portable blocking pool explicitly.
#[test]
fn forced_blocking_backend_serves() {
    let www = Www::build();
    let server = EnvoyServer::terminal(json!({
        "root": www.path().to_str().unwrap(),
        "force_blocking": true,
    }));

    let hello = server.get("/hello.txt");
    assert_eq!(hello.status, 200);
    assert_eq!(hello.body, b"hello world\n");

    let data = server.request("GET", "/data.bin", &[("Range", "bytes=0-9")]);
    assert_eq!(data.status, 206);
    assert_eq!(data.body, (0u8..10).collect::<Vec<u8>>());
}
