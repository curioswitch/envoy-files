use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;

use envoy_files_core::listing::{self, DirEntry};
use envoy_files_core::mime::content_type_for;
use envoy_files_core::path::{PathPolicy, RejectReason, ValidatedPath, resolve_request_path};
use envoy_files_core::range::RangePolicy;
use envoy_files_core::response_plan::{
    BodyPlan, FileMeta, RequestFacts, ResponsePlan, plan_response,
};
use envoy_files_core::validators::ConditionalHeaders;
use envoy_files_io::{DirEntryInfo, FileHandle, FileStat, IoEngine, IoError, OpenRequest};
use envoy_proxy_dynamic_modules_rust_sdk::{
    EnvoyHttpFilter, EnvoyHttpFilterScheduler, HttpFilter, abi, envoy_log_error,
};

use crate::config::{Config, DirectoryMode, RangeSupport};
use crate::eventbridge::EventBridge;

const EVENT_ID_IO: u64 = 1;

/// Wakes the worker thread from an I/O thread: push the event, then commit.
/// A push failure means the stream was torn down, so the event (and any owned
/// buffer it carries) is simply dropped here.
#[derive(Clone)]
struct Waker {
    bridge: EventBridge<IoEvent>,
    scheduler: Arc<dyn EnvoyHttpFilterScheduler>,
}

impl Waker {
    fn deliver(&self, event: IoEvent) {
        if self.bridge.send(event).is_ok() {
            self.scheduler.commit(EVENT_ID_IO);
        }
    }
}

/// Context threaded through an open so its completion can be planned without
/// re-deriving anything on the worker thread.
struct OpenCtx {
    encoding: Option<&'static str>,
    content_type: String,
    add_vary: bool,
}

enum IoEvent {
    Opened {
        handle: FileHandle,
        stat: FileStat,
        ctx: OpenCtx,
        /// Bytes prefetched from offset 0 during the open (empty if none).
        prefetch: Vec<u8>,
    },
    OpenError {
        error: IoError,
    },
    Read {
        seq: usize,
        result: Result<Vec<u8>, IoError>,
    },
    Dir {
        result: Result<Vec<DirEntryInfo>, IoError>,
    },
}

struct Candidate {
    fs_path: PathBuf,
    encoding: Option<&'static str>,
    content_type: String,
    add_vary: bool,
}

/// One ordered unit of the response body: either bytes we already have
/// (multipart framing) or a file range still to be read.
enum Chunk {
    Inline(Vec<u8>),
    File { offset: u64, len: usize },
}

struct Streaming {
    handle: FileHandle,
    chunks: Vec<Chunk>,
    ready: HashMap<usize, Vec<u8>>,
    next_send: usize,
    issue_cursor: usize,
    inflight: usize,
    /// Downstream high/low watermark callbacks nest, so this is a depth
    /// counter, not a flag; streaming resumes only when it returns to zero.
    watermark_depth: usize,
}

enum Phase {
    Idle,
    ResolvingFile {
        pending: VecDeque<Candidate>,
    },
    ResolvingIndex {
        dir_fs: PathBuf,
        pending: VecDeque<(PathBuf, String)>,
    },
    Streaming(Streaming),
    Done,
}

struct RequestData {
    method_head: bool,
    if_none_match: Option<String>,
    if_modified_since: Option<String>,
    if_match: Option<String>,
    if_unmodified_since: Option<String>,
    if_range: Option<String>,
    range_header: Option<String>,
    relative: String,
    trailing_slash: bool,
}

pub struct Filter {
    config: Arc<Config>,
    engine: Arc<dyn IoEngine>,
    bridge: EventBridge<IoEvent>,
    waker: Option<Waker>,
    request: Option<RequestData>,
    phase: Phase,
}

impl Filter {
    pub fn new(config: Arc<Config>, engine: Arc<dyn IoEngine>) -> Self {
        Self {
            config,
            engine,
            bridge: EventBridge::new(),
            waker: None,
            request: None,
            phase: Phase::Idle,
        }
    }
}

impl<EHF: EnvoyHttpFilter> HttpFilter<EHF> for Filter {
    fn on_request_headers(
        &mut self,
        envoy_filter: &mut EHF,
        _end_of_stream: bool,
    ) -> abi::envoy_dynamic_module_type_on_http_filter_request_headers_status {
        use abi::envoy_dynamic_module_type_on_http_filter_request_headers_status as Status;
        use http::{Method, header};

        let method = request_method(envoy_filter);
        let method_head = method.as_ref() == Some(&Method::HEAD);
        if !method_head && method.as_ref() != Some(&Method::GET) {
            self.phase = Phase::Done;
            envoy_filter.send_response(405, &[("allow", b"GET, HEAD".as_slice())], None, None);
            return Status::StopIteration;
        }

        let raw_path = header_string(envoy_filter, ":path").unwrap_or_default();
        let raw_path = if let Some(prefix) = self.config.strip_prefix.as_deref() {
            match strip_mount_prefix(&raw_path, prefix) {
                Some(stripped) => stripped,
                None => {
                    // Path isn't under this mount's prefix; nothing to serve.
                    self.respond_status(envoy_filter, http::StatusCode::NOT_FOUND);
                    return Status::StopIteration;
                }
            }
        } else {
            raw_path
        };
        let policy = PathPolicy {
            serve_dotfiles: self.config.serve_dotfiles,
            max_path_len: 4096,
        };
        let (relative, trailing_slash) = match resolve_request_path(&raw_path, &policy) {
            ValidatedPath::Ok {
                relative,
                trailing_slash,
            } => (relative, trailing_slash),
            ValidatedPath::Reject(reason) => {
                let status = match reason {
                    RejectReason::TooLong => http::StatusCode::URI_TOO_LONG,
                    RejectReason::BadEncoding | RejectReason::BadCharacter => {
                        http::StatusCode::BAD_REQUEST
                    }
                    RejectReason::Traversal | RejectReason::Dotfile => http::StatusCode::NOT_FOUND,
                };
                self.respond_status(envoy_filter, status);
                return Status::StopIteration;
            }
        };

        let accept_encoding = header_string(envoy_filter, header::ACCEPT_ENCODING.as_str());
        self.request = Some(RequestData {
            method_head,
            if_none_match: header_string(envoy_filter, header::IF_NONE_MATCH.as_str()),
            if_modified_since: header_string(envoy_filter, header::IF_MODIFIED_SINCE.as_str()),
            if_match: header_string(envoy_filter, header::IF_MATCH.as_str()),
            if_unmodified_since: header_string(envoy_filter, header::IF_UNMODIFIED_SINCE.as_str()),
            if_range: header_string(envoy_filter, header::IF_RANGE.as_str()),
            range_header: header_string(envoy_filter, header::RANGE.as_str()),
            relative: relative.clone(),
            trailing_slash,
        });

        self.waker = Some(Waker {
            bridge: self.bridge.clone(),
            scheduler: Arc::new(envoy_filter.new_scheduler()),
        });

        let pending = self.build_file_candidates(&relative, trailing_slash, accept_encoding);
        self.phase = Phase::ResolvingFile { pending };
        self.open_next_candidate(envoy_filter);
        Status::StopIteration
    }

    fn on_scheduled(&mut self, envoy_filter: &mut EHF, event_id: u64) {
        debug_assert_eq!(event_id, EVENT_ID_IO);
        let mut events = Vec::new();
        self.bridge.process(|event| events.push(event));
        for event in events {
            if self.handle_event(envoy_filter, event) {
                return;
            }
        }
        if matches!(self.phase, Phase::Streaming(_)) {
            self.pump(envoy_filter);
        }
    }

    fn on_downstream_above_write_buffer_high_watermark(&mut self, _envoy_filter: &mut EHF) {
        if let Phase::Streaming(streaming) = &mut self.phase {
            streaming.watermark_depth += 1;
        }
    }

    fn on_downstream_below_write_buffer_low_watermark(&mut self, _envoy_filter: &mut EHF) {
        let resumed = match &mut self.phase {
            Phase::Streaming(streaming) => {
                streaming.watermark_depth = streaming.watermark_depth.saturating_sub(1);
                streaming.watermark_depth == 0
            }
            _ => false,
        };
        if resumed && let Some(waker) = &self.waker {
            waker.scheduler.commit(EVENT_ID_IO);
        }
    }

    fn on_stream_complete(&mut self, _envoy_filter: &mut EHF) {
        // Fence any in-flight I/O: later completions fail their bridge push and
        // drop their buffers on the I/O thread.
        self.bridge.close();
    }
}

impl Filter {
    fn build_file_candidates(
        &self,
        relative: &str,
        trailing_slash: bool,
        accept_encoding: Option<String>,
    ) -> VecDeque<Candidate> {
        // Owned because it's carried through the async open (across the I/O
        // thread) in each Candidate/OpenCtx; the config borrow ends here.
        let content_type = content_type_for(
            relative,
            &self.config.mime_overrides,
            &self.config.default_content_type,
        )
        .to_owned();
        let mut candidates = VecDeque::new();

        // Precompressed variants only make sense for a concrete file request
        // (not a directory), and only when the client accepts an encoding.
        if !trailing_slash && let Some(accept) = accept_encoding.as_deref() {
            let accepted = envoy_files_core::encoding::negotiate_precompressed(
                Some(accept),
                &self.config.precompressed,
            );
            for encoding in accepted {
                candidates.push_back(Candidate {
                    fs_path: self
                        .config
                        .root
                        .join(format!("{relative}{}", encoding.file_suffix)),
                    encoding: Some(encoding.content_encoding),
                    content_type: content_type.clone(),
                    add_vary: true,
                });
            }
        }

        let add_vary = !candidates.is_empty();
        candidates.push_back(Candidate {
            fs_path: self.config.root.join(relative),
            encoding: None,
            content_type,
            add_vary,
        });
        candidates
    }

    fn open_next_candidate<EHF: EnvoyHttpFilter>(&mut self, envoy_filter: &mut EHF) -> bool {
        let Phase::ResolvingFile { pending } = &mut self.phase else {
            return false;
        };
        let Some(candidate) = pending.pop_front() else {
            return self.respond_status(envoy_filter, http::StatusCode::NOT_FOUND);
        };
        let ctx = OpenCtx {
            encoding: candidate.encoding,
            content_type: candidate.content_type,
            add_vary: candidate.add_vary,
        };
        // submit_open only queues an async open; the stream is not ended here.
        self.submit_open(candidate.fs_path, ctx);
        false
    }

    fn open_next_index<EHF: EnvoyHttpFilter>(&mut self, envoy_filter: &mut EHF) -> bool {
        let Phase::ResolvingIndex { pending, dir_fs } = &mut self.phase else {
            return false;
        };
        match pending.pop_front() {
            Some((fs_path, content_type)) => {
                let ctx = OpenCtx {
                    encoding: None,
                    content_type,
                    add_vary: false,
                };
                self.submit_open(fs_path, ctx);
                false
            }
            None => {
                let dir_fs = dir_fs.clone();
                self.directory_fallback(envoy_filter, dir_fs)
            }
        }
    }

    fn submit_open(&self, fs_path: PathBuf, ctx: OpenCtx) {
        let waker = self.waker.clone().expect("waker set before open");
        // Fold the first chunk into the open for a plain whole-file GET (the
        // response streams from offset 0). Skip for HEAD (no body) and range
        // requests (body doesn't start at 0), where the prefetch would be waste.
        let prefetch_len = match &self.request {
            Some(req) if !req.method_head && req.range_header.is_none() => self.config.chunk_size,
            _ => 0,
        };
        let request = OpenRequest {
            path: fs_path,
            containment_root: Some(self.config.root.clone()),
            deny_symlinks: !self.config.follow_symlinks_within_root,
            prefetch_len,
        };
        self.engine.open_stat(
            request,
            Box::new(move |result| {
                let event = match result {
                    Ok((handle, stat, prefetch)) => IoEvent::Opened {
                        handle,
                        stat,
                        ctx,
                        prefetch,
                    },
                    Err(error) => {
                        drop(ctx);
                        IoEvent::OpenError { error }
                    }
                };
                waker.deliver(event);
            }),
        );
    }

    /// Dispatches one I/O event. Returns `true` if it ended the stream (any
    /// terminal send) — after which Envoy may have destroyed this filter, so the
    /// caller must not touch `self`.
    fn handle_event<EHF: EnvoyHttpFilter>(
        &mut self,
        envoy_filter: &mut EHF,
        event: IoEvent,
    ) -> bool {
        match event {
            IoEvent::Opened {
                handle,
                stat,
                ctx,
                prefetch,
            } => self.handle_opened(envoy_filter, handle, stat, ctx, prefetch),
            IoEvent::OpenError { error } => self.handle_open_error(envoy_filter, error),
            IoEvent::Read { seq, result } => self.handle_read(envoy_filter, seq, result),
            IoEvent::Dir { result } => self.handle_dir(envoy_filter, result),
        }
    }

    fn handle_opened<EHF: EnvoyHttpFilter>(
        &mut self,
        envoy_filter: &mut EHF,
        handle: FileHandle,
        stat: FileStat,
        ctx: OpenCtx,
        prefetch: Vec<u8>,
    ) -> bool {
        if stat.is_dir {
            if ctx.encoding.is_some() {
                // A precompressed path resolved to a directory; ignore it.
                return self.advance_after_open_miss(envoy_filter);
            }
            return self.handle_directory(envoy_filter);
        }
        self.serve_file(envoy_filter, handle, stat, ctx, prefetch)
    }

    fn handle_open_error<EHF: EnvoyHttpFilter>(
        &mut self,
        envoy_filter: &mut EHF,
        error: IoError,
    ) -> bool {
        match &self.phase {
            Phase::ResolvingFile { pending } if !pending.is_empty() => {
                self.open_next_candidate(envoy_filter)
            }
            Phase::ResolvingFile { .. } => {
                let status =
                    envoy_files_core::error::status_for_io_error(error.kind, error.raw_os_error);
                self.respond_status(envoy_filter, status)
            }
            Phase::ResolvingIndex { .. } => self.open_next_index(envoy_filter),
            _ => false,
        }
    }

    fn advance_after_open_miss<EHF: EnvoyHttpFilter>(&mut self, envoy_filter: &mut EHF) -> bool {
        match &self.phase {
            Phase::ResolvingFile { .. } => self.open_next_candidate(envoy_filter),
            Phase::ResolvingIndex { .. } => self.open_next_index(envoy_filter),
            _ => false,
        }
    }

    fn handle_directory<EHF: EnvoyHttpFilter>(&mut self, envoy_filter: &mut EHF) -> bool {
        let request = self.request.as_ref().expect("request set");
        let relative = request.relative.clone();
        let trailing_slash = request.trailing_slash;

        if !trailing_slash && self.config.redirect_trailing_slash {
            let location = format!("{}/", self.url_for(&relative));
            self.phase = Phase::Done;
            envoy_filter.send_response(301, &[("location", location.as_bytes())], None, None);
            return true;
        }

        if self.config.directory == DirectoryMode::Deny {
            return self.respond_status(envoy_filter, http::StatusCode::FORBIDDEN);
        }

        let dir_fs = self.config.root.join(&relative);
        let pending: VecDeque<(PathBuf, String)> = self
            .config
            .index_files
            .iter()
            .map(|name| {
                let content_type = content_type_for(
                    name,
                    &self.config.mime_overrides,
                    &self.config.default_content_type,
                )
                .to_owned();
                (dir_fs.join(name), content_type)
            })
            .collect();

        if pending.is_empty() {
            return self.directory_fallback(envoy_filter, dir_fs);
        }
        self.phase = Phase::ResolvingIndex { dir_fs, pending };
        self.open_next_index(envoy_filter)
    }

    fn directory_fallback<EHF: EnvoyHttpFilter>(
        &mut self,
        envoy_filter: &mut EHF,
        dir_fs: PathBuf,
    ) -> bool {
        match self.config.directory {
            DirectoryMode::Listing => {
                // The listing is sent later, in handle_dir; this only kicks off
                // the async read_dir, so the stream is not ended here.
                let waker = self.waker.clone().expect("waker set");
                self.engine.read_dir(
                    dir_fs,
                    Box::new(move |result| waker.deliver(IoEvent::Dir { result })),
                );
                false
            }
            DirectoryMode::Index | DirectoryMode::Deny => {
                self.respond_status(envoy_filter, http::StatusCode::NOT_FOUND)
            }
        }
    }

    fn handle_dir<EHF: EnvoyHttpFilter>(
        &mut self,
        envoy_filter: &mut EHF,
        result: Result<Vec<DirEntryInfo>, IoError>,
    ) -> bool {
        let entries = match result {
            Ok(entries) => entries,
            Err(error) => {
                let status =
                    envoy_files_core::error::status_for_io_error(error.kind, error.raw_os_error);
                return self.respond_status(envoy_filter, status);
            }
        };
        let relative = self.request.as_ref().expect("request set").relative.clone();
        let url_path = self.url_for(&relative);
        let at_root = relative.is_empty();
        let dir_entries: Vec<DirEntry> = entries
            .into_iter()
            .map(|e| DirEntry {
                name: e.name,
                is_dir: e.is_dir,
                size: e.size,
                mtime_unix: e.mtime_unix,
            })
            .collect();
        let html = listing::render_listing(&url_path, at_root, &dir_entries);
        self.phase = Phase::Done;
        envoy_filter.send_response(
            200,
            &[("content-type", b"text/html; charset=utf-8".as_slice())],
            Some(html.as_bytes()),
            None,
        );
        true
    }

    fn serve_file<EHF: EnvoyHttpFilter>(
        &mut self,
        envoy_filter: &mut EHF,
        handle: FileHandle,
        stat: FileStat,
        ctx: OpenCtx,
        prefetch: Vec<u8>,
    ) -> bool {
        let request = self.request.as_ref().expect("request set");
        let range_header = if self.config.ranges == RangeSupport::None {
            None
        } else {
            request.range_header.as_deref()
        };
        let boundary = format!(
            "envoyfilesboundary{:016x}{:016x}",
            stat.size, stat.mtime_unix as u64
        );
        let facts = RequestFacts {
            method_head: request.method_head,
            conditionals: ConditionalHeaders {
                if_none_match: request.if_none_match.as_deref(),
                if_modified_since: request.if_modified_since.as_deref(),
                if_match: request.if_match.as_deref(),
                if_unmodified_since: request.if_unmodified_since.as_deref(),
                if_range: request.if_range.as_deref(),
            },
            range_header,
            content_type: ctx.content_type,
            etag_enabled: self.config.etag,
            last_modified_enabled: self.config.last_modified,
            range_policy: RangePolicy {
                allow_multi: self.config.ranges == RangeSupport::Multi,
                max_parts: 50,
            },
            boundary,
            content_encoding: ctx.encoding,
            add_vary: ctx.add_vary,
            cache_control: self.config.cache_control.as_deref(),
        };
        let meta = FileMeta {
            size: stat.size,
            mtime_unix: stat.mtime_unix,
            mtime_nanos: stat.mtime_nanos,
            is_dir: false,
        };
        let plan = plan_response(&facts, &meta);
        self.execute_plan(envoy_filter, plan, handle, prefetch)
    }

    fn execute_plan<EHF: EnvoyHttpFilter>(
        &mut self,
        envoy_filter: &mut EHF,
        plan: ResponsePlan,
        handle: FileHandle,
        prefetch: Vec<u8>,
    ) -> bool {
        let status = plan.status;
        let mut headers: Vec<(&str, &[u8])> = Vec::with_capacity(plan.headers.len() + 1);
        headers.push((":status", status.as_str().as_bytes()));
        headers.extend(header_refs(&plan.headers));

        if let BodyPlan::Empty = plan.body {
            self.phase = Phase::Done;
            envoy_filter.send_response_headers(&headers, true);
            return true;
        }

        // The prefetch (if any) holds bytes [0, chunk_size) and is only valid
        // for a body that streams from offset 0 — i.e. a Whole plan, which is
        // also the only case submit_open requested a prefetch for.
        let prefetch = match plan.body {
            BodyPlan::Whole { .. } => prefetch,
            _ => Vec::new(),
        };

        let chunks = match plan.body {
            BodyPlan::Empty => unreachable!("handled above"),
            BodyPlan::Whole { len } => self.file_chunks(0, len),
            BodyPlan::Segments(ranges) => {
                let mut chunks = Vec::new();
                for range in ranges {
                    chunks.extend(self.file_chunks(range.start, range.end - range.start + 1));
                }
                chunks
            }
            BodyPlan::Multipart {
                parts, terminator, ..
            } => {
                let mut chunks = Vec::new();
                for part in parts {
                    chunks.push(Chunk::Inline(part.header));
                    chunks.extend(
                        self.file_chunks(part.range.start, part.range.end - part.range.start + 1),
                    );
                }
                chunks.push(Chunk::Inline(terminator));
                chunks
            }
        };
        self.begin_streaming(envoy_filter, &headers, handle, chunks, prefetch)
    }

    fn file_chunks(&self, start: u64, len: u64) -> Vec<Chunk> {
        let chunk_size = self.config.chunk_size as u64;
        let mut chunks = Vec::new();
        let mut offset = start;
        let end = start + len;
        while offset < end {
            let this = (end - offset).min(chunk_size);
            chunks.push(Chunk::File {
                offset,
                len: this as usize,
            });
            offset += this;
        }
        chunks
    }

    fn begin_streaming<EHF: EnvoyHttpFilter>(
        &mut self,
        envoy_filter: &mut EHF,
        headers: &[(&str, &[u8])],
        handle: FileHandle,
        chunks: Vec<Chunk>,
        prefetch: Vec<u8>,
    ) -> bool {
        if chunks.is_empty() {
            self.phase = Phase::Done;
            envoy_filter.send_response_headers(headers, true);
            return true;
        }

        let mut ready = HashMap::new();
        for (seq, chunk) in chunks.iter().enumerate() {
            if let Chunk::Inline(bytes) = chunk {
                ready.insert(seq, bytes.clone());
            }
        }
        // Seed chunk 0 from the prefetch when it exactly covers that file chunk,
        // so pump can flush it without issuing a read (the read-issue loop skips
        // chunks already in `ready`).
        if let Some(Chunk::File { offset: 0, len }) = chunks.first()
            && prefetch.len() == *len
        {
            ready.insert(0, prefetch);
        }
        self.phase = Phase::Streaming(Streaming {
            handle,
            chunks,
            ready,
            next_send: 0,
            issue_cursor: 0,
            inflight: 0,
            watermark_depth: 0,
        });
        // Headers with end_stream=false do not end the stream; on_scheduled
        // issues the initial reads and flushes after this event is handled.
        envoy_filter.send_response_headers(headers, false);
        false
    }

    fn handle_read<EHF: EnvoyHttpFilter>(
        &mut self,
        envoy_filter: &mut EHF,
        seq: usize,
        result: Result<Vec<u8>, IoError>,
    ) -> bool {
        let Phase::Streaming(streaming) = &mut self.phase else {
            return false;
        };
        streaming.inflight = streaming.inflight.saturating_sub(1);
        match result {
            Ok(data) => {
                streaming.ready.insert(seq, data);
                // The flush + read-window refill is driven by on_scheduled once
                // every event in this batch is handled.
                false
            }
            Err(error) => {
                // Headers are already sent, so the only signal left is to reset
                // the stream by ending it early.
                envoy_log_error!(
                    "envoy-files: read failed mid-stream at seq {seq} \
                     (kind={:?} errno={:?}); truncating response",
                    error.kind,
                    error.raw_os_error
                );
                self.phase = Phase::Done;
                envoy_filter.send_response_data(&[], true);
                true
            }
        }
    }

    fn pump<EHF: EnvoyHttpFilter>(&mut self, envoy_filter: &mut EHF) {
        let max_inflight = self.config.max_inflight_reads;

        // Flush contiguous ready chunks. Crucially, no borrow of `self.phase`
        // is held across a `send_response_data` call: Envoy may reentrantly
        // invoke a watermark hook (which touches `self.phase`), and after an
        // end-of-stream send we must not touch `self` at all.
        loop {
            let next = match &mut self.phase {
                Phase::Streaming(streaming) if streaming.watermark_depth == 0 => {
                    let last_index = streaming.chunks.len() - 1;
                    match streaming.ready.remove(&streaming.next_send) {
                        Some(bytes) => {
                            let end_stream = streaming.next_send == last_index;
                            streaming.next_send += 1;
                            Some((bytes, end_stream))
                        }
                        None => None,
                    }
                }
                _ => return,
            };
            match next {
                Some((bytes, true)) => {
                    self.phase = Phase::Done;
                    envoy_filter.send_response_data(&bytes, true);
                    return;
                }
                Some((bytes, false)) => {
                    envoy_filter.send_response_data(&bytes, false);
                }
                None => break,
            }
        }

        // Issue reads for upcoming file chunks up to the in-flight window.
        // engine.read does not call back into Envoy, so holding the borrow
        // here is safe.
        let waker = self.waker.clone().expect("waker set");
        let mut requests = Vec::new();
        {
            let Phase::Streaming(streaming) = &mut self.phase else {
                return;
            };
            while streaming.inflight < max_inflight
                && streaming.issue_cursor < streaming.chunks.len()
            {
                let index = streaming.issue_cursor;
                streaming.issue_cursor += 1;
                // Skip chunks already satisfied (inline framing, or chunk 0 seeded
                // from the open's prefetch) so they aren't read a second time.
                if streaming.ready.contains_key(&index) {
                    continue;
                }
                if let Chunk::File { offset, len } = streaming.chunks[index] {
                    streaming.inflight += 1;
                    requests.push((index, offset, len, streaming.handle.clone()));
                }
            }
        }
        for (index, offset, len, handle) in requests {
            let waker = waker.clone();
            self.engine.read(
                &handle,
                offset,
                len,
                Box::new(move |result| waker.deliver(IoEvent::Read { seq: index, result })),
            );
        }
    }

    /// Maps a resolved root-relative path back to a request URL path,
    /// restoring the stripped mount prefix so redirects and listing headings
    /// point back through the mount. With no `strip_prefix`, this is just the
    /// path with a leading slash.
    fn url_for(&self, relative: &str) -> String {
        let prefix = self.config.strip_prefix.as_deref().unwrap_or("");
        if relative.is_empty() {
            format!("{prefix}/")
        } else {
            format!("{prefix}/{relative}")
        }
    }

    /// Sends a bodyless status response. Only called before any response
    /// headers have been sent. Phase is advanced before the send because the
    /// send ends the stream and `self` must not be touched afterward. Always
    /// returns `true` (the stream is ended) for callers threading that signal.
    fn respond_status<EHF: EnvoyHttpFilter>(
        &mut self,
        envoy_filter: &mut EHF,
        status: http::StatusCode,
    ) -> bool {
        self.phase = Phase::Done;
        envoy_filter.send_response(status.as_u16() as u32, &[], None, None);
        true
    }
}

/// Removes a normalized mount prefix (leading slash, no trailing slash) from
/// the raw request path at a segment boundary, so `/static` matches `/static`
/// and `/static/x` but not `/staticfoo`. Returns the remainder (always
/// starting with `/`), or `None` if the path is not under the prefix.
fn strip_mount_prefix(raw_path: &str, prefix: &str) -> Option<String> {
    let rest = raw_path.strip_prefix(prefix)?;
    match rest.as_bytes().first() {
        None => Some("/".to_string()),
        Some(b'/') => Some(rest.to_string()),
        Some(b'?') | Some(b'#') => Some(format!("/{rest}")),
        _ => None,
    }
}

fn request_method<EHF: EnvoyHttpFilter>(envoy_filter: &EHF) -> Option<http::Method> {
    let value = envoy_filter.get_request_header_value(":method")?;
    http::Method::from_bytes(value.as_slice()).ok()
}

fn header_string<EHF: EnvoyHttpFilter>(envoy_filter: &EHF, key: &str) -> Option<String> {
    envoy_filter
        .get_request_header_value(key)
        .map(|value| String::from_utf8_lossy(value.as_slice()).into_owned())
        .filter(|value| !value.is_empty())
}

fn header_refs(headers: &[(http::HeaderName, http::HeaderValue)]) -> Vec<(&str, &[u8])> {
    headers
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_bytes()))
        .collect()
}
