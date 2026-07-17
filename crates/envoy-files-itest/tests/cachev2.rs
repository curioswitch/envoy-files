use envoy_files_itest::{EnvoyServer, Www, cachev2_composition_config};
use serde_json::json;

#[test]
fn cachev2_serves_module_then_cache_hit() {
    let www = Www::build();
    let config = cachev2_composition_config(json!({
        "root": www.path().to_str().unwrap(),
        "cache_control": "max-age=300",
    }));
    let server = EnvoyServer::with_config(config, false);

    // First request: cache miss, fetched from the module via the internal
    // listener.
    let first = server.get("/hello.txt");
    assert_eq!(first.status, 200);
    assert_eq!(first.body, b"hello world\n");
    assert_eq!(first.header("cache-control"), Some("max-age=300"));

    // Second request: served from the CacheV2 cache.
    let second = server.get("/hello.txt");
    assert_eq!(second.status, 200);
    assert_eq!(second.body, b"hello world\n");

    // The module (behind the internal-listener cluster) was hit exactly once;
    // the second response came from cache.
    let upstream_rq = server.admin_stat("cluster.files_internal.upstream_rq_total");
    assert_eq!(upstream_rq, 1, "expected exactly one upstream request");
}
