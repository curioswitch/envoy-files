//! Asynchronous file I/O engine. Completions are delivered on threads owned
//! by the engine and routed back to Envoy worker threads by the caller (via
//! the Envoy scheduler); this crate has no Envoy SDK dependency.

mod engine;

pub use engine::FileHandle;

use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug)]
pub struct FileStat {
    pub size: u64,
    pub mtime_unix: i64,
    pub mtime_nanos: u32,
    pub is_dir: bool,
}

#[derive(Debug)]
pub struct DirEntryInfo {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime_unix: i64,
}

#[derive(Debug)]
pub struct IoError {
    pub kind: std::io::ErrorKind,
    pub raw_os_error: Option<i32>,
}

impl From<std::io::Error> for IoError {
    fn from(e: std::io::Error) -> Self {
        Self {
            kind: e.kind(),
            raw_os_error: e.raw_os_error(),
        }
    }
}

pub struct OpenRequest {
    pub path: PathBuf,
    /// Canonicalized root the resolved file must stay within. Symlinks are
    /// followed, then containment is verified against the canonical path.
    pub containment_root: Option<PathBuf>,
    /// Refuse to open through a symlink at the final component.
    pub deny_symlinks: bool,
}

pub type OpenStatCallback = Box<dyn FnOnce(Result<(FileHandle, FileStat), IoError>) + Send>;
/// The buffer's length is the number of bytes read; empty means EOF.
pub type ReadCallback = Box<dyn FnOnce(Result<Vec<u8>, IoError>) + Send>;
pub type ReadDirCallback = Box<dyn FnOnce(Result<Vec<DirEntryInfo>, IoError>) + Send>;

/// A file I/O backend. Submission methods never block the calling (Envoy
/// worker) thread; callbacks run on engine-owned threads and must hand off to
/// the worker thread themselves (bridge push + scheduler commit).
pub trait IoEngine: Send + Sync + 'static {
    fn open_stat(&self, request: OpenRequest, on_done: OpenStatCallback);
    fn read(&self, file: &FileHandle, offset: u64, len: usize, on_done: ReadCallback);
    fn read_dir(&self, path: PathBuf, on_done: ReadDirCallback);
    fn backend_name(&self) -> &'static str;
    /// Stops accepting work and joins engine threads. Pending callbacks run
    /// or are dropped before this returns; safe to call once during config
    /// teardown.
    fn shutdown(&self);
}

pub struct IoConfig {
    pub blocking_threads: usize,
    pub force_blocking: bool,
}

impl Default for IoConfig {
    fn default() -> Self {
        Self {
            blocking_threads: std::thread::available_parallelism()
                .map(|n| n.get().min(4))
                .unwrap_or(2),
            force_blocking: false,
        }
    }
}

/// Starts the I/O engine: a single `compio` runtime that transparently uses
/// io_uring (Linux), IOCP (Windows), or a polling driver backed by a blocking
/// pool (macOS, or Linux without io_uring). `force_blocking` pins it to the
/// polling driver where io_uring would otherwise be chosen.
pub fn select_backend(config: &IoConfig) -> std::io::Result<Arc<dyn IoEngine>> {
    let engine = engine::CompioEngine::try_new(config.blocking_threads, config.force_blocking)?;
    Ok(Arc::new(engine))
}
