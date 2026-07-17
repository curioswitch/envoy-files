use std::cell::RefCell;
use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use async_channel::{Receiver, Sender};
use compio_buf::BufResult;
use compio_driver::{DriverType, ProactorBuilder};
use compio_fs::{File, OpenOptions};
use compio_io::AsyncReadAt;
use compio_runtime::{Runtime, RuntimeBuilder};

use crate::{
    DirEntryInfo, FileStat, IoError, OpenRequest, OpenStatCallback, ReadCallback, ReadDirCallback,
};

/// An open file living on the engine's runtime thread. The handle that crosses
/// to Envoy worker threads is just an id (plus a channel to request close);
/// the `compio_fs::File` itself is `!Send` and never leaves the runtime thread.
/// Cloning bumps a refcount; the file is closed on the runtime thread when the
/// last clone drops.
#[derive(Clone, Debug)]
pub struct FileHandle(std::sync::Arc<HandleInner>);

#[derive(Debug)]
struct HandleInner {
    id: u64,
    closer: Sender<Command>,
}

impl Drop for HandleInner {
    fn drop(&mut self) {
        // Ask the runtime thread to drop (and thus close) the file. This fails
        // silently after shutdown or when the file was already closed with the
        // runtime.
        let _ = self.closer.try_send(Command::Close { id: self.id });
    }
}

enum Command {
    Open {
        id: u64,
        request: OpenRequest,
        on_done: OpenStatCallback,
    },
    Read {
        id: u64,
        offset: u64,
        len: usize,
        on_done: ReadCallback,
        queued: std::time::Instant,
    },
    ReadDir {
        path: PathBuf,
        on_done: ReadDirCallback,
    },
    Close {
        id: u64,
    },
    Shutdown,
}

/// The I/O engine that serves file operations.
pub(crate) struct CompioEngine {
    commands: Sender<Command>,
    reactor: Mutex<Option<std::thread::JoinHandle<()>>>,
    next_id: AtomicU64,
    driver: &'static str,
}

impl CompioEngine {
    pub(crate) fn try_new(blocking_threads: usize, force_blocking: bool) -> std::io::Result<Self> {
        let (commands, rx) = async_channel::unbounded::<Command>();
        // A oneshot just to report the driver type (or a startup error) back
        // once the runtime has been built on the reactor thread.
        let (init_tx, init_rx) = oneshot::channel::<std::io::Result<&'static str>>();
        let closer = commands.clone();
        let reactor = std::thread::Builder::new()
            .name("envoy-files-compio".to_string())
            .spawn(move || {
                let rt = match build_runtime(blocking_threads, force_blocking) {
                    Ok(rt) => rt,
                    Err(error) => {
                        let _ = init_tx.send(Err(error));
                        return;
                    }
                };
                let driver = match rt.driver_type() {
                    DriverType::IoUring => "compio-io_uring",
                    DriverType::IOCP => "compio-iocp",
                    DriverType::Poll => "compio-poll",
                };
                if init_tx.send(Ok(driver)).is_err() {
                    return;
                }
                run_reactor(rt, rx, closer);
            })?;

        let driver = match init_rx.recv() {
            Ok(Ok(driver)) => driver,
            Ok(Err(error)) => {
                let _ = reactor.join();
                return Err(error);
            }
            Err(_) => {
                let _ = reactor.join();
                return Err(std::io::Error::other("compio runtime failed to start"));
            }
        };

        Ok(Self {
            commands,
            reactor: Mutex::new(Some(reactor)),
            next_id: AtomicU64::new(1),
            driver,
        })
    }
}

impl crate::IoEngine for CompioEngine {
    fn open_stat(&self, request: OpenRequest, on_done: OpenStatCallback) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        // Unbounded send never blocks the worker thread; it only fails after
        // shutdown, when the owned callback is simply dropped.
        let _ = self.commands.try_send(Command::Open {
            id,
            request,
            on_done,
        });
    }

    fn read(&self, file: &FileHandle, offset: u64, len: usize, on_done: ReadCallback) {
        let _ = self.commands.try_send(Command::Read {
            id: file.0.id,
            offset,
            len,
            on_done,
            queued: std::time::Instant::now(),
        });
    }

    fn read_dir(&self, path: PathBuf, on_done: ReadDirCallback) {
        let _ = self.commands.try_send(Command::ReadDir { path, on_done });
    }

    fn backend_name(&self) -> &'static str {
        self.driver
    }

    fn shutdown(&self) {
        let _ = self.commands.try_send(Command::Shutdown);
        if let Some(handle) = self.reactor.lock().unwrap().take() {
            let _ = handle.join();
        }
    }
}

fn build_runtime(blocking_threads: usize, force_blocking: bool) -> std::io::Result<Runtime> {
    let mut proactor = ProactorBuilder::new();
    proactor.thread_pool_limit(blocking_threads.max(1));
    // io_uring is the only completion driver we'd want to force off; it exists
    // only on Linux. macOS already uses the polling driver, and Windows has
    // only IOCP (no polling driver to fall back to).
    #[cfg(target_os = "linux")]
    if force_blocking {
        proactor.driver_type(DriverType::Poll);
    }
    #[cfg(not(target_os = "linux"))]
    let _ = force_blocking;

    let mut builder = RuntimeBuilder::new();
    builder.with_proactor(proactor);
    builder.build()
}

fn run_reactor(rt: Runtime, rx: Receiver<Command>, closer: Sender<Command>) {
    rt.block_on(async move {
        let files: Rc<RefCell<HashMap<u64, Rc<File>>>> = Rc::new(RefCell::new(HashMap::new()));
        loop {
            let command = match rx.recv().await {
                Ok(command) => command,
                // All senders dropped: nothing more can arrive.
                Err(_) => break,
            };
            match command {
                Command::Shutdown => break,
                Command::Close { id } => {
                    // Dropping the last `Rc<File>` closes the descriptor. Reads
                    // still in flight hold their own clone, so the close is
                    // deferred until they finish.
                    let t = std::time::Instant::now();
                    files.borrow_mut().remove(&id);
                    let ms = t.elapsed().as_millis();
                    eprintln!("engine: close id={id} remove_ms={ms}");
                }
                Command::Open {
                    id,
                    request,
                    on_done,
                } => {
                    let files = files.clone();
                    let closer = closer.clone();
                    compio_runtime::spawn(async move {
                        on_done(open_stat(id, request, &files, closer).await);
                    })
                    .detach();
                }
                Command::Read {
                    id,
                    offset,
                    len,
                    on_done,
                    queued,
                } => {
                    let file = files.borrow().get(&id).cloned();
                    compio_runtime::spawn(async move {
                        let result = read_at(file, offset, len).await;
                        let ms = queued.elapsed().as_millis();
                        if ms > 20 {
                            eprintln!("engine: SLOW read id={id} offset={offset} latency_ms={ms}");
                        }
                        on_done(result);
                    })
                    .detach();
                }
                Command::ReadDir { path, on_done } => {
                    compio_runtime::spawn(async move {
                        let result = compio_runtime::spawn_blocking(move || read_dir(&path))
                            .await
                            .unwrap_or_else(|_| Err(other_error()));
                        on_done(result);
                    })
                    .detach();
                }
            }
        }
    });
}

async fn open_stat(
    id: u64,
    request: OpenRequest,
    files: &Rc<RefCell<HashMap<u64, Rc<File>>>>,
    closer: Sender<Command>,
) -> Result<(FileHandle, FileStat), IoError> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    if request.deny_symlinks {
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(&request.path).await?;
    let metadata = file.metadata().await?;

    if let Some(root) = &request.containment_root {
        // Symlinks may point anywhere; the canonical location of what we opened
        // must still be inside the root. `canonicalize` is blocking, so it goes
        // to the runtime's blocking pool rather than stalling the reactor.
        let path = request.path.clone();
        let canonical = compio_runtime::spawn_blocking(move || std::fs::canonicalize(&path))
            .await
            .unwrap_or_else(|_| Err(std::io::Error::from(ErrorKind::Other)))?;
        if !canonical.starts_with(root) {
            return Err(IoError {
                kind: ErrorKind::PermissionDenied,
                raw_os_error: None,
            });
        }
    }

    let stat = stat_from_metadata(&metadata);
    files.borrow_mut().insert(id, Rc::new(file));
    let handle = FileHandle(std::sync::Arc::new(HandleInner { id, closer }));
    Ok((handle, stat))
}

async fn read_at(file: Option<Rc<File>>, offset: u64, len: usize) -> Result<Vec<u8>, IoError> {
    let Some(file) = file else {
        // The handle was closed before this read ran.
        return Err(IoError {
            kind: ErrorKind::NotFound,
            raw_os_error: None,
        });
    };
    // A single `read_at` may return fewer bytes than requested (a short read is
    // legal on every backend). The caller expects exactly `len` bytes for a
    // chunk, so loop until it's filled or EOF — otherwise the gap is silently
    // dropped and the response ends up shorter than its Content-Length.
    // `read_at` returns the buffer with its length set to the bytes read.
    let BufResult(result, mut buf) = file.read_at(Vec::with_capacity(len), offset).await;
    let first = result?;
    if first == len || first == 0 {
        return Ok(buf); // Full read (the common case) or immediate EOF.
    }
    while buf.len() < len {
        let want = len - buf.len();
        let at = offset + buf.len() as u64;
        let BufResult(result, more) = file.read_at(Vec::with_capacity(want), at).await;
        let n = result?;
        if n == 0 {
            break; // EOF before `len`: return the true prefix.
        }
        buf.extend_from_slice(&more[..n]);
    }
    Ok(buf)
}

fn read_dir(path: &Path) -> Result<Vec<DirEntryInfo>, IoError> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        entries.push(DirEntryInfo {
            name,
            is_dir: metadata.is_dir(),
            size: metadata.len(),
            mtime_unix: mtime_parts(metadata.modified()).0,
        });
    }
    Ok(entries)
}

fn stat_from_metadata(metadata: &compio_fs::Metadata) -> FileStat {
    let (mtime_unix, mtime_nanos) = mtime_parts(metadata.modified());
    FileStat {
        size: metadata.len(),
        mtime_unix,
        mtime_nanos,
        is_dir: metadata.is_dir(),
    }
}

fn mtime_parts(modified: std::io::Result<SystemTime>) -> (i64, u32) {
    match modified.map(|time| time.duration_since(UNIX_EPOCH)) {
        Ok(Ok(duration)) => (duration.as_secs() as i64, duration.subsec_nanos()),
        _ => (0, 0),
    }
}

fn other_error() -> IoError {
    IoError {
        kind: ErrorKind::Other,
        raw_os_error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::IoEngine;
    use std::sync::mpsc;

    fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "envoy-files-engine-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn engine() -> CompioEngine {
        CompioEngine::try_new(2, false).unwrap()
    }

    #[test]
    fn open_stat_and_out_of_order_reads() {
        let dir = tempdir();
        let path = dir.join("data.bin");
        std::fs::write(&path, b"0123456789").unwrap();

        let engine = engine();
        assert!(engine.backend_name().starts_with("compio-"));

        let (tx, rx) = mpsc::channel();
        engine.open_stat(
            OpenRequest {
                path,
                containment_root: None,
                deny_symlinks: false,
            },
            Box::new(move |result| tx.send(result).unwrap()),
        );
        let (handle, stat) = rx.recv().unwrap().unwrap();
        assert_eq!(stat.size, 10);
        assert!(!stat.is_dir);

        // Two reads issued back to back, completing in either order.
        let (tx, rx) = mpsc::channel();
        engine.read(&handle, 5, 5, Box::new(move |r| tx.send(r).unwrap()));
        let (tx2, rx2) = mpsc::channel();
        engine.read(&handle, 0, 4, Box::new(move |r| tx2.send(r).unwrap()));
        assert_eq!(rx.recv().unwrap().unwrap(), b"56789");
        assert_eq!(rx2.recv().unwrap().unwrap(), b"0123");

        // Reading past EOF returns the available prefix, then empty.
        let (tx, rx) = mpsc::channel();
        engine.read(&handle, 8, 10, Box::new(move |r| tx.send(r).unwrap()));
        assert_eq!(rx.recv().unwrap().unwrap(), b"89");
        let (tx, rx) = mpsc::channel();
        engine.read(&handle, 10, 4, Box::new(move |r| tx.send(r).unwrap()));
        assert_eq!(rx.recv().unwrap().unwrap(), b"");

        engine.shutdown();
    }

    #[test]
    fn open_missing_file_maps_not_found() {
        let engine = engine();
        let (tx, rx) = mpsc::channel();
        engine.open_stat(
            OpenRequest {
                path: PathBuf::from("/nonexistent/does/not/exist"),
                containment_root: None,
                deny_symlinks: false,
            },
            Box::new(move |result| tx.send(result).unwrap()),
        );
        let err = rx.recv().unwrap().unwrap_err();
        assert_eq!(err.kind, ErrorKind::NotFound);
        engine.shutdown();
    }

    #[test]
    fn read_dir_lists_entries() {
        let dir = tempdir();
        let sub = dir.join("listing");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("a.txt"), b"a").unwrap();
        std::fs::write(sub.join("b.txt"), b"bb").unwrap();

        let engine = engine();
        let (tx, rx) = mpsc::channel();
        engine.read_dir(sub, Box::new(move |result| tx.send(result).unwrap()));
        let mut names: Vec<String> = rx
            .recv()
            .unwrap()
            .unwrap()
            .into_iter()
            .map(|entry| entry.name)
            .collect();
        names.sort();
        assert_eq!(names, vec!["a.txt".to_string(), "b.txt".to_string()]);
        engine.shutdown();
    }
}
