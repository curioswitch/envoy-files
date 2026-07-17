use envoy_files_itest::{EnvoyServer, Www};
use serde_json::json;

#[test]
fn serves_a_file() {
    let www = Www::build();
    let server = EnvoyServer::terminal(json!({"root": www.path().to_str().unwrap()}));
    let response = server.get("/hello.txt");
    assert_eq!(response.status, 200);
    assert_eq!(response.body, b"hello world\n");
}
