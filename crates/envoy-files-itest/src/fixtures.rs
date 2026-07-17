//! Builds the document-root fixture tree used by the tests.

use std::io::Write;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

pub const BIG_FILE_BYTES: usize = 100 * 1024 * 1024;

/// A fixture document root, created fresh under the OS temp dir.
pub struct Www {
    pub root: PathBuf,
    pub big_sha256: String,
}

impl Www {
    pub fn build() -> Www {
        let root = std::env::temp_dir().join(format!(
            "envoy-files-www-{}-{}",
            std::process::id(),
            unique()
        ));
        std::fs::create_dir_all(&root).expect("create www");

        write(&root, "index.html", b"<html>home</html>");
        write(&root, "hello.txt", b"hello world\n");
        // data.bin: 10240 bytes, values 0..256 repeated, for range tests.
        let data: Vec<u8> = (0..10240).map(|i| (i % 256) as u8).collect();
        write(&root, "data.bin", &data);
        write(&root, ".secret", b"dotfile");

        std::fs::create_dir_all(root.join("sub")).unwrap();
        write(&root, "sub/index.html", b"<html>sub</html>");
        write(&root, "sub/nested.txt", b"nested");

        std::fs::create_dir_all(root.join("listing-dir")).unwrap();
        write(&root, "listing-dir/a.txt", b"a");
        write(&root, "listing-dir/<evil>.txt", b"x");

        let js = b"console.log('envoy files');\n".repeat(10);
        write(&root, "app.js", &js);
        write(&root, "app.js.gz", &gzip(&js));
        // Not real brotli; only negotiation + byte passthrough are asserted.
        write(&root, "app.js.br", b"BROTLI-BYTES");

        // big.bin: deterministic fast fill (not random), so writing 100 MiB is
        // cheap and the sha256 is reproducible.
        let big_sha256 = write_big(&root.join("big.bin"));

        // Symlinks: one escaping the root (denied), one internal (allowed).
        #[cfg(unix)]
        {
            let outside = root.parent().unwrap().join(format!(
                "envoy-files-outside-{}-{}",
                std::process::id(),
                unique()
            ));
            std::fs::write(&outside, b"outside").unwrap();
            std::os::unix::fs::symlink(&outside, root.join("escape.txt")).unwrap();
            std::os::unix::fs::symlink(root.join("hello.txt"), root.join("inside-link.txt"))
                .unwrap();
        }

        Www { root, big_sha256 }
    }

    pub fn path(&self) -> &Path {
        &self.root
    }
}

fn write(root: &Path, rel: &str, bytes: &[u8]) {
    std::fs::write(root.join(rel), bytes).unwrap_or_else(|e| panic!("write {rel}: {e}"));
}

fn write_big(path: &Path) -> String {
    let mut file = std::io::BufWriter::new(std::fs::File::create(path).expect("create big.bin"));
    let mut hasher = Sha256::new();
    let block: Vec<u8> = (0..65536).map(|i| (i % 251) as u8).collect();
    let mut written = 0;
    while written < BIG_FILE_BYTES {
        let n = block.len().min(BIG_FILE_BYTES - written);
        file.write_all(&block[..n]).expect("write big block");
        hasher.update(&block[..n]);
        written += n;
    }
    file.flush().expect("flush big.bin");
    format!("{:x}", hasher.finalize())
}

fn gzip(data: &[u8]) -> Vec<u8> {
    use flate2::Compression;
    use flate2::write::GzEncoder;
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

fn unique() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}
