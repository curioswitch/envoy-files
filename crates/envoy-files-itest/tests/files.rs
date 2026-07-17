use std::sync::LazyLock;

use envoy_files_itest::{EnvoyServer, Www};
use serde_json::json;
use sha2::{Digest, Sha256};

// One server serves the whole file, shared across the tests in this binary
// (the equivalent of a module-scoped pytest fixture).
static SERVER: LazyLock<(Www, EnvoyServer)> = LazyLock::new(|| {
    let www = Www::build();
    let server = EnvoyServer::terminal(json!({
        "root": www.path().to_str().unwrap(),
        "directory": "listing",
    }));
    (www, server)
});

fn server() -> &'static EnvoyServer {
    &SERVER.1
}

#[test]
fn get_text_file() {
    let r = server().get("/hello.txt");
    assert_eq!(r.status, 200);
    assert_eq!(r.body, b"hello world\n");
    assert_eq!(r.header("content-type"), Some("text/plain"));
    assert_eq!(r.header("content-length"), Some("12"));
    assert_eq!(r.header("accept-ranges"), Some("bytes"));
    assert!(r.header("etag").unwrap().starts_with('"'));
    assert!(r.header("last-modified").is_some());
}

#[test]
fn head_matches_get_without_body() {
    let get = server().get("/hello.txt");
    let head = server().request("HEAD", "/hello.txt", &[]);
    assert_eq!(head.status, 200);
    assert!(head.body.is_empty());
    assert_eq!(head.header("content-length"), get.header("content-length"));
    assert_eq!(head.header("etag"), get.header("etag"));
}

#[test]
fn not_found() {
    assert_eq!(server().get("/nope.txt").status, 404);
}

#[test]
fn method_not_allowed() {
    let r = server().request("POST", "/hello.txt", &[]);
    assert_eq!(r.status, 405);
    assert_eq!(r.header("allow"), Some("GET, HEAD"));
}

#[test]
fn index_file_for_root() {
    let r = server().get("/");
    assert_eq!(r.status, 200);
    assert_eq!(r.body, b"<html>home</html>");
    assert_eq!(r.header("content-type"), Some("text/html"));
}

#[test]
fn directory_redirects_to_trailing_slash() {
    let r = server().get("/sub");
    assert_eq!(r.status, 301);
    assert_eq!(r.header("location"), Some("/sub/"));
}

#[test]
fn directory_index_file() {
    let r = server().get("/sub/");
    assert_eq!(r.status, 200);
    assert_eq!(r.body, b"<html>sub</html>");
}

#[test]
fn directory_listing_escapes_html() {
    let r = server().get("/listing-dir/");
    assert_eq!(r.status, 200);
    assert!(r.header("content-type").unwrap().starts_with("text/html"));
    let body = String::from_utf8_lossy(&r.body);
    assert!(!body.contains("<evil>"));
    assert!(body.contains("&lt;evil&gt;"));
    assert!(body.contains("a.txt"));
}

#[test]
fn dotfile_denied() {
    assert_eq!(server().get("/.secret").status, 404);
}

#[test]
fn traversal_rejected() {
    for path in [
        "/../hello.txt",
        "/%2e%2e/hello.txt",
        "/..%2fhello.txt",
        "/sub/%2e%2e/%2e%2e/hello.txt",
        "/con.txt",
        "/hello.txt%00",
        "/a%5cb.txt",
        "/file.txt:$DATA",
    ] {
        let status = server().get(path).status;
        assert!(status == 400 || status == 404, "path {path} gave {status}");
    }
}

#[cfg(unix)]
#[test]
fn symlink_escape_denied_internal_allowed() {
    let escape = server().get("/escape.txt").status;
    assert!(escape == 403 || escape == 404, "escape gave {escape}");
    let inside = server().get("/inside-link.txt");
    assert_eq!(inside.status, 200);
    assert_eq!(inside.body, b"hello world\n");
}

#[test]
fn big_file_streams_intact() {
    let r = server().get("/big.bin");
    assert_eq!(r.status, 200);
    assert_eq!(
        r.header("content-length")
            .unwrap()
            .parse::<usize>()
            .unwrap(),
        r.body.len()
    );
    let digest = format!("{:x}", Sha256::digest(&r.body));
    assert_eq!(digest, SERVER.0.big_sha256);
}

#[test]
fn conditional_etag_304() {
    let etag = server()
        .get("/hello.txt")
        .header("etag")
        .unwrap()
        .to_string();
    let r = server().request("GET", "/hello.txt", &[("If-None-Match", &etag)]);
    assert_eq!(r.status, 304);
    assert!(r.body.is_empty());
    assert_eq!(r.header("etag"), Some(etag.as_str()));
}

#[test]
fn conditional_last_modified_304() {
    let last_modified = server()
        .get("/hello.txt")
        .header("last-modified")
        .unwrap()
        .to_string();
    let r = server().request(
        "GET",
        "/hello.txt",
        &[("If-Modified-Since", &last_modified)],
    );
    assert_eq!(r.status, 304);
}

#[test]
fn if_match_mismatch_412() {
    let r = server().request("GET", "/hello.txt", &[("If-Match", "\"bogus\"")]);
    assert_eq!(r.status, 412);
}

#[test]
fn single_range() {
    let r = server().request("GET", "/data.bin", &[("Range", "bytes=0-9")]);
    assert_eq!(r.status, 206);
    assert_eq!(r.body, (0u8..10).collect::<Vec<u8>>());
    assert_eq!(r.header("content-range"), Some("bytes 0-9/10240"));
    assert_eq!(r.header("content-length"), Some("10"));
}

#[test]
fn suffix_range() {
    let r = server().request("GET", "/data.bin", &[("Range", "bytes=-16")]);
    assert_eq!(r.status, 206);
    assert_eq!(r.body, (240u8..=255).collect::<Vec<u8>>());
    assert_eq!(r.header("content-range"), Some("bytes 10224-10239/10240"));
}

#[test]
fn open_ended_range() {
    let r = server().request("GET", "/data.bin", &[("Range", "bytes=10200-")]);
    assert_eq!(r.status, 206);
    assert_eq!(r.body.len(), 40);
}

#[test]
fn range_unsatisfiable_416() {
    let r = server().request("GET", "/data.bin", &[("Range", "bytes=99999-")]);
    assert_eq!(r.status, 416);
    assert_eq!(r.header("content-range"), Some("bytes */10240"));
}

#[test]
fn multi_range_multipart() {
    let r = server().request("GET", "/data.bin", &[("Range", "bytes=0-1,4-5")]);
    assert_eq!(r.status, 206);
    let content_type = r.header("content-type").unwrap();
    let boundary = content_type
        .strip_prefix("multipart/byteranges; boundary=")
        .expect("multipart content-type");
    assert_eq!(
        r.header("content-length")
            .unwrap()
            .parse::<usize>()
            .unwrap(),
        r.body.len()
    );
    let sep = format!("--{boundary}");
    let parts: Vec<&[u8]> = split_on(&r.body, sep.as_bytes());
    // leading empty, two parts, trailing "--\r\n"
    assert_eq!(parts.len(), 4, "unexpected multipart part count");
    assert!(window_contains(parts[1], b"Content-Range: bytes 0-1/10240"));
    assert!(window_contains(parts[1], &[0u8, 1]));
    assert!(window_contains(parts[2], &[4u8, 5]));
}

#[test]
fn if_range_stale_etag_serves_full() {
    let r = server().request(
        "GET",
        "/data.bin",
        &[("Range", "bytes=0-1"), ("If-Range", "\"stale\"")],
    );
    assert_eq!(r.status, 200);
    assert_eq!(r.body.len(), 10240);
}

#[test]
fn precompressed_gzip() {
    let r = server().request("GET", "/app.js", &[("Accept-Encoding", "gzip")]);
    assert_eq!(r.status, 200);
    assert_eq!(r.header("content-encoding"), Some("gzip"));
    assert_eq!(
        r.header("vary").map(|v| v.to_ascii_lowercase()),
        Some("accept-encoding".into())
    );
    assert!(r.header("content-type").unwrap().contains("javascript"));

    use flate2::read::GzDecoder;
    use std::io::Read as _;
    let mut decoded = Vec::new();
    GzDecoder::new(&r.body[..])
        .read_to_end(&mut decoded)
        .unwrap();
    assert_eq!(decoded, b"console.log('envoy files');\n".repeat(10));
}

#[test]
fn precompressed_brotli_preferred() {
    let r = server().request("GET", "/app.js", &[("Accept-Encoding", "br, gzip")]);
    assert_eq!(r.status, 200);
    assert_eq!(r.header("content-encoding"), Some("br"));
    assert_eq!(r.body, b"BROTLI-BYTES");
}

#[test]
fn precompressed_identity_without_accept_encoding() {
    let r = server().get("/app.js");
    assert_eq!(r.status, 200);
    assert_eq!(r.header("content-encoding"), None);
    assert_eq!(r.body, b"console.log('envoy files');\n".repeat(10));
}

fn split_on<'a>(haystack: &'a [u8], sep: &[u8]) -> Vec<&'a [u8]> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i + sep.len() <= haystack.len() {
        if &haystack[i..i + sep.len()] == sep {
            parts.push(&haystack[start..i]);
            i += sep.len();
            start = i;
        } else {
            i += 1;
        }
    }
    parts.push(&haystack[start..]);
    parts
}

fn window_contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}
