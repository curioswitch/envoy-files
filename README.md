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
never reaches it. Set `strip_prefix` on each mount so the filter strips it before
resolving against `root` — `/static/app.css` with `strip_prefix: /static` is
served from `<root>/app.css`.

## Performance

Benchmarks are run against every commit in the [bench](./.github/workflows/bench.yaml)
workflow. GitHub action runners are highly virtualized and do not have stable
performance across runs, but the relative numbers within a run should still be
somewhat, though not precisely, informative. Always measure your own workloads,
these are to get an idea of performance compared to other approaches.
One run looks like this:

```text
envoy-files (compio)  [io backend: compio-io_uring]
  /index.html    25914.8 rps   p50=   1.87ms   p99=   3.44ms   2xx=1554940
  /home.html     24498.5 rps   p50=   1.99ms   p99=   3.62ms   2xx=1469913
  /big.bin          35.2 rps   p50= 114.15ms   p99= 129.90ms   2xx=2108   ~3513 MiB/s   ttfb=0.24ms
  /index+big     19733.3 rps   p50=   2.46ms   p99=   4.51ms   2xx=1183990   (+4 in-flight /big.bin)
built-in file_server
  /index.html    31373.2 rps   p50=   1.54ms   p99=   2.96ms   2xx=1882463
  /home.html     30212.2 rps   p50=   1.61ms   p99=   3.07ms   2xx=1812781
  /big.bin          21.0 rps   p50= 190.58ms   p99= 209.61ms   2xx=1256   ~2093 MiB/s   ttfb=0.23ms
  /index+big     26447.0 rps   p50=   1.83ms   p99=   3.62ms   2xx=1586862   (+4 in-flight /big.bin)
boe file-server
  /index.html    26173.4 rps   p50=   1.80ms   p99=   4.43ms   2xx=1570466
  /home.html     22181.6 rps   p50=   2.12ms   p99=   5.38ms   2xx=1330969
  /big.bin          42.0 rps   p50=  85.02ms   p99= 190.39ms   2xx=2515   ~4192 MiB/s   ttfb=20.45ms
  /index+big      7333.4 rps   p50=   3.27ms   p99=  53.86ms   2xx=439972   (+4 in-flight /big.bin)
```

When comparing to the built-in file server, we see that for small payloads that fit within a single
chunk, envoy-files is about a 15% slower. This is because there is minimal I/O overhead for small
files that fit in the OS's file cache, and most time is in dispatching from the worker back
to Envoy, which has unavoidable overhead due to the dynamic modules mechanism vs a native
filter. On the flip side, for a large file, envoy-files is about 50% faster, reflecting
the improved I/O performance from asynchronous I/O. In general, envoy-files seems to provide
high and stable performance across any file size, with greater benefit for very large files.

[boe's file-server](https://builtonenvoy.io/extensions/file-server/) is another dynamic module
option, written in Go. However, it seems to buffer the entire file in memory on the Envoy worker
thread. We see this with high p99 latencies and time-to-first-byte, with the latter also indicating
poor performance in high-latency (i.e. external web server) environments due to no backpressure.
Due to blocking the Envoy's worker thread, we can also see the case with small and big files
served together having issues due to worker starvation. We will continue to include it
in case the architecture improves in the future, but for now it seems to not be production-ready
like the other two options.

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
