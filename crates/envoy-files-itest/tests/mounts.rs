//! Serving the module at multiple path prefixes via Envoy's matching API,
//! with `strip_prefix` mapping each mount back onto one document root.

use envoy_files_itest::{EnvoyServer, Www, mounts_config};
use serde_json::json;

#[test]
fn multiple_mounts_strip_their_prefix() {
    let www = Www::build();
    let root = www.path().to_str().unwrap();
    // Two independent registrations over the same tree, each mounted at its own
    // prefix. `strip_prefix` duplicates the matcher key.
    let server = EnvoyServer::with_config(
        mounts_config(&[
            (
                "/static/",
                json!({"root": root, "strip_prefix": "/static/", "directory": "listing"}),
            ),
            (
                "/assets/",
                json!({"root": root, "strip_prefix": "/assets/"}),
            ),
        ]),
        false,
    );

    // Each mount strips its prefix and resolves under the shared root.
    let a = server.get("/static/hello.txt");
    assert_eq!(a.status, 200);
    assert_eq!(a.body, b"hello world\n");

    // The second registration is served independently by the same module.
    let b = server.get("/assets/hello.txt");
    assert_eq!(b.status, 200);
    assert_eq!(b.body, b"hello world\n");

    // Nested paths resolve under the mount.
    let nested = server.get("/static/sub/nested.txt");
    assert_eq!(nested.status, 200);
    assert_eq!(nested.body, b"nested");

    // The mount root serves the index file.
    let index = server.get("/static/");
    assert_eq!(index.status, 200);
    assert_eq!(index.body, b"<html>home</html>");

    // Trailing-slash redirect keeps the mount prefix in Location (the key
    // strip_prefix round-trip: a bare `/static/sub` -> `/static/sub/`, not
    // `/sub/`, which would escape the mount).
    let redirect = server.request("GET", "/static/sub", &[]);
    assert_eq!(redirect.status, 301);
    assert_eq!(redirect.header("location"), Some("/static/sub/"));

    // Directory listing heading reflects the full (prefixed) URL path.
    let listing = server.get("/static/listing-dir/");
    assert_eq!(listing.status, 200);
    let html = String::from_utf8_lossy(&listing.body);
    assert!(
        html.contains("Index of /static/listing-dir"),
        "listing heading should carry the mount prefix: {html}"
    );

    // A path outside every mount misses the matcher and falls through to the
    // router's catch-all — i.e. the module coexists with ordinary routing.
    let miss = server.get("/nope");
    assert_eq!(miss.status, 404);
}
