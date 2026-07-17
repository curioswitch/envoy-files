# envoy-files

An Envoy [dynamic module](https://www.envoyproxy.io/docs/envoy/latest/intro/arch_overview/advanced/dynamic_modules)
HTTP filter, written in Rust, that serves static files from the local
filesystem with complete HTTP semantics.

Unlike Envoy's built in file-server, where possible, asynchronous I/O is used for
file operations, using io_uring on Linux and IOCP on Windows. On other platforms,
blocking I/O on a threadpool is used. And yes, the above means the module works
on Windows if using a custom Envoy build on it like from [py-envoy-server](https://github.com/curioswitch/py-envoy-server).

## Features

- GET/HEAD, correct `405` with `Allow` for other methods.
- Content-type mapping from filenames
- Conditional requests: strong `ETag` + `Last-Modified`,
  `If-None-Match`/`If-Modified-Since` → `304`, `If-Match`/`If-Unmodified-Since`
  → `412`, `If-Range`.
- Range requests: single, suffix (`bytes=-N`), and multi-range
  (`multipart/byteranges`), with an anti-amplification part cap.
- Precompressed assets: serves `foo.js.br` / `foo.js.gz` with the right
  `Content-Encoding` + `Vary` when the client accepts it (q-value aware).
- Directory handling: index files, trailing-slash redirect, and an opt-in
  generated HTML listing.
- Chunked streaming with backpressure and a bounded in-flight read window.
- Path-traversal hardening, including Windows device-name / ADS / backslash
  defenses.

## Building

```sh
cargo build --release
```

## Configuration

| Key | Type | Default | Meaning |
|-----|------|---------|---------|
| `root` | string (**required**) | — | Document root. Canonicalized at load; must be an existing directory. |
| `strip_prefix` | string | _(none)_ | URL prefix stripped before resolving against `root`; restored on redirects/listings. For mounting at a path prefix — see [Deploying](#deploying). |
| `index_files` | list of string | `["index.html"]` | Files tried, in order, for a directory request. |
| `directory` | `index` \| `listing` \| `deny` | `index` | On a directory with no matching index: `index` → 404, `listing` → generated HTML listing, `deny` → 403. |
| `redirect_trailing_slash` | bool | `true` | Redirect `GET /dir` → `301 /dir/`. |
| `follow_symlinks_within_root` | bool | `true` | If false, refuse to open through a symlink (`O_NOFOLLOW`). When true, symlinks are followed but the resolved path must stay within `root`. |
| `serve_dotfiles` | bool | `false` | Serve path segments beginning with `.`. |
| `chunk_size` | int (4096..=4 MiB) | `65536` | Bytes per streamed read. |
| `max_inflight_reads` | int (1..=8) | `2` | Reads issued ahead of the send cursor (bounds per-stream buffering to `max_inflight_reads × chunk_size`). |
| `blocking_threads` | int (1..=256) | `min(cpus, 4)` | Size of the runtime's blocking pool (directory listing, and all file I/O when the polling driver is active). |
| `force_blocking` | bool | `false` | Disable the compio backend; use the portable thread pool. |
| `etag` | bool | `true` | Emit `ETag` and honor `If-None-Match`/`If-Match`. |
| `last_modified` | bool | `true` | Emit `Last-Modified` and honor `If-Modified-Since`/`If-Unmodified-Since`. |
| `ranges` | `none` \| `single` \| `multi` | `multi` | Range support level. Multi-range bodies are capped at 50 parts. |
| `precompressed` | list of `br` \| `gzip` \| `zstd` | `[br, gzip]` | Precompressed variants to serve, in server preference order. |
| `mime_overrides` | map string→string | `{}` | Extension → content-type overrides (keys with or without leading dot). |
| `default_content_type` | string | `application/octet-stream` | Content-type for unknown extensions. |
| `cache_control` | string | _(none)_ | `Cache-Control` value emitted on 200/206 (makes CacheV2 composition deterministic). |

## Deploying

Prebuilt modules for Linux, macOS, and Windows, each with a `.sha256`, are
attached to every GitHub release — use them with Envoy's remote
(HTTP) data source, which fetches by URL and verifies the checksum, or download
one directly.

Otherwise, place the built artifact in a directory on
`ENVOY_DYNAMIC_MODULES_SEARCH_PATH` (named `libenvoy_files.so` on POSIX,
`envoy_files.dll` on Windows) and configure the filter as a terminal HTTP
filter:

```yaml
http_filters:
  - name: envoy_files
    typed_config:
      "@type": type.googleapis.com/envoy.extensions.filters.http.dynamic_modules.v3.DynamicModuleFilter
      dynamic_module_config: { name: envoy_files }
      filter_name: envoy_files
      terminal_filter: true
      filter_config:
        "@type": type.googleapis.com/google.protobuf.StringValue
        value: '{"root": "/var/www", "directory": "index"}'
```

### Serving alongside other backends (multiple roots)

To serve static folders at path prefixes and proxy the rest to an
application from one listener, select a per-prefix `filter_config` with Envoy's
matching API (`ExtensionWithMatcher` + a `:path` `prefix_match_map`). 
Matched prefixes run envoy_files as a terminal filter (responding directly);
unmatched paths fall through to the router.
See [testing/envoy/mounts-with-app.yaml](testing/envoy/mounts-with-app.yaml)
for a complete config.

A terminal filter responds before the router, so route-level `prefix_rewrite`
never reaches it. Set `strip_prefix` on each moun so the filter strips it before
resolving against `root` — `/static/app.css` with `strip_prefix: /static` is
served from `<root>/app.css`.

## Testing

- Unit tests (pure logic + I/O engine):
  `cargo test --workspace --exclude envoy-files-itest`
- Integration tests against a real Envoy: build the module, then run the tests:
  `cargo build -p envoy-files-filter && cargo test -p envoy-files-itest`.
  The [itest crate](crates/envoy-files-itest) fetches the Envoy binary from the
  `envoy-server` PyPI wheel (cached under `target/`), locates the prebuilt
  module matching the test binary's profile, stages it, and drives real HTTP
  requests. (CI runs the build step explicitly; the harness never builds.)
- Benchmark (oha must be installed) — build the module in release to match:
  `cargo build -p envoy-files-filter --release && cargo run -p envoy-files-itest --example bench --release`.
  Also runs in CI on pushes to `main` ([bench.yaml](.github/workflows/bench.yaml)).
