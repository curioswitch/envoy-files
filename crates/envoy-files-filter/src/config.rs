use std::path::PathBuf;

use envoy_files_core::encoding::{self, Encoding};
use yaml_rust2::{Yaml, YamlLoader};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectoryMode {
    /// Try index files; 404 otherwise.
    Index,
    /// Try index files, then render a generated listing.
    Listing,
    /// Directories are always 404.
    Deny,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeSupport {
    None,
    Single,
    Multi,
}

#[derive(Debug)]
pub struct Config {
    /// Canonicalized at load time; containment checks compare against this.
    pub root: PathBuf,
    pub strip_prefix: Option<String>,
    pub index_files: Vec<String>,
    pub directory: DirectoryMode,
    pub redirect_trailing_slash: bool,
    pub follow_symlinks_within_root: bool,
    pub serve_dotfiles: bool,
    pub chunk_size: usize,
    pub max_inflight_reads: usize,
    pub blocking_threads: Option<usize>,
    pub force_blocking: bool,
    pub etag: bool,
    pub last_modified: bool,
    pub ranges: RangeSupport,
    pub precompressed: Vec<Encoding>,
    pub mime_overrides: Vec<(String, String)>,
    pub default_content_type: String,
    pub cache_control: Option<String>,
}

const KNOWN_KEYS: &[&str] = &[
    "root",
    "strip_prefix",
    "index_files",
    "directory",
    "redirect_trailing_slash",
    "follow_symlinks_within_root",
    "serve_dotfiles",
    "chunk_size",
    "max_inflight_reads",
    "blocking_threads",
    "force_blocking",
    "etag",
    "last_modified",
    "ranges",
    "precompressed",
    "mime_overrides",
    "default_content_type",
    "cache_control",
];

pub fn parse(raw: &[u8]) -> Result<Config, String> {
    let text = std::str::from_utf8(raw).map_err(|_| "config is not valid UTF-8".to_string())?;
    let docs =
        YamlLoader::load_from_str(text).map_err(|e| format!("config is not valid YAML: {e}"))?;
    let doc = docs.first().ok_or("config is empty")?;

    if let Yaml::Hash(hash) = doc {
        for key in hash.keys() {
            let Yaml::String(key) = key else {
                return Err("config keys must be strings".to_string());
            };
            if !KNOWN_KEYS.contains(&key.as_str()) {
                return Err(format!("unknown config key: {key}"));
            }
        }
    } else {
        return Err("config must be a map".to_string());
    }

    let root = doc["root"]
        .as_str()
        .ok_or("missing required config key: root")?;
    let root =
        std::fs::canonicalize(root).map_err(|e| format!("root {root:?} is not accessible: {e}"))?;
    if !root.is_dir() {
        return Err(format!("root {root:?} is not a directory"));
    }

    let strip_prefix = match &doc["strip_prefix"] {
        Yaml::BadValue => None,
        Yaml::String(prefix) => {
            let trimmed = prefix.trim_end_matches('/');
            if trimmed.is_empty() {
                return Err("strip_prefix must be a non-empty path prefix".to_string());
            }
            Some(if trimmed.starts_with('/') {
                trimmed.to_string()
            } else {
                format!("/{trimmed}")
            })
        }
        _ => return Err("strip_prefix must be a string".to_string()),
    };

    let index_files = match &doc["index_files"] {
        Yaml::BadValue => vec!["index.html".to_string()],
        Yaml::Array(values) => values
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_string)
                    .ok_or("index_files entries must be strings".to_string())
            })
            .collect::<Result<_, _>>()?,
        _ => return Err("index_files must be a list".to_string()),
    };

    let directory = match doc["directory"].as_str() {
        None => DirectoryMode::Index,
        Some("index") => DirectoryMode::Index,
        Some("listing") => DirectoryMode::Listing,
        Some("deny") => DirectoryMode::Deny,
        Some(other) => return Err(format!("unknown directory mode: {other}")),
    };

    let ranges = match doc["ranges"].as_str() {
        None => RangeSupport::Multi,
        Some("none") => RangeSupport::None,
        Some("single") => RangeSupport::Single,
        Some("multi") => RangeSupport::Multi,
        Some(other) => return Err(format!("unknown ranges mode: {other}")),
    };

    let precompressed = match &doc["precompressed"] {
        Yaml::BadValue => vec![encoding::BROTLI, encoding::GZIP],
        Yaml::Array(values) => values
            .iter()
            .map(|v| match v.as_str() {
                Some("br") => Ok(encoding::BROTLI),
                Some("gzip") => Ok(encoding::GZIP),
                Some("zstd") => Ok(encoding::ZSTD),
                _ => Err("precompressed entries must be br, gzip, or zstd".to_string()),
            })
            .collect::<Result<_, _>>()?,
        _ => return Err("precompressed must be a list".to_string()),
    };

    let mime_overrides = match &doc["mime_overrides"] {
        Yaml::BadValue => Vec::new(),
        Yaml::Hash(hash) => {
            let mut overrides = Vec::new();
            for (key, value) in hash {
                match (key.as_str(), value.as_str()) {
                    (Some(k), Some(v)) => overrides.push((k.to_string(), v.to_string())),
                    _ => return Err("mime_overrides must map strings to strings".to_string()),
                }
            }
            overrides
        }
        _ => return Err("mime_overrides must be a map".to_string()),
    };

    let chunk_size = int_in_range(doc, "chunk_size", 65536, 4096, 4 << 20)?;
    let max_inflight_reads = int_in_range(doc, "max_inflight_reads", 2, 1, 8)?;

    let blocking_threads = match &doc["blocking_threads"] {
        Yaml::BadValue => None,
        value => Some(
            value
                .as_i64()
                .filter(|&n| (1..=256).contains(&n))
                .ok_or("blocking_threads must be an integer in 1..=256")? as usize,
        ),
    };

    Ok(Config {
        root,
        strip_prefix,
        index_files,
        directory,
        redirect_trailing_slash: bool_or(doc, "redirect_trailing_slash", true)?,
        follow_symlinks_within_root: bool_or(doc, "follow_symlinks_within_root", true)?,
        serve_dotfiles: bool_or(doc, "serve_dotfiles", false)?,
        chunk_size,
        max_inflight_reads,
        blocking_threads,
        force_blocking: bool_or(doc, "force_blocking", false)?,
        etag: bool_or(doc, "etag", true)?,
        last_modified: bool_or(doc, "last_modified", true)?,
        ranges,
        precompressed,
        mime_overrides,
        default_content_type: doc["default_content_type"]
            .as_str()
            .unwrap_or("application/octet-stream")
            .to_string(),
        cache_control: doc["cache_control"].as_str().map(str::to_string),
    })
}

fn bool_or(doc: &Yaml, key: &str, default: bool) -> Result<bool, String> {
    match &doc[key] {
        Yaml::BadValue => Ok(default),
        value => value.as_bool().ok_or(format!("{key} must be a boolean")),
    }
}

fn int_in_range(doc: &Yaml, key: &str, default: i64, min: i64, max: i64) -> Result<usize, String> {
    match &doc[key] {
        Yaml::BadValue => Ok(default as usize),
        value => Ok(value
            .as_i64()
            .filter(|n| (min..=max).contains(n))
            .ok_or(format!("{key} must be an integer in {min}..={max}"))?
            as usize),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root_dir() -> String {
        std::env::temp_dir().to_string_lossy().into_owned()
    }

    #[test]
    fn defaults() {
        let config = parse(format!(r#"{{"root": "{}"}}"#, root_dir()).as_bytes()).unwrap();
        assert_eq!(config.index_files, vec!["index.html"]);
        assert_eq!(config.directory, DirectoryMode::Index);
        assert_eq!(config.chunk_size, 65536);
        assert_eq!(config.max_inflight_reads, 2);
        assert_eq!(config.ranges, RangeSupport::Multi);
        assert_eq!(config.precompressed, vec![encoding::BROTLI, encoding::GZIP]);
        assert!(config.etag);
        assert!(!config.serve_dotfiles);
    }

    #[test]
    fn json_is_accepted_as_yaml() {
        let raw = format!(
            r#"{{"root": "{}", "directory": "listing", "ranges": "single", "chunk_size": 8192}}"#,
            root_dir()
        );
        let config = parse(raw.as_bytes()).unwrap();
        assert_eq!(config.directory, DirectoryMode::Listing);
        assert_eq!(config.ranges, RangeSupport::Single);
        assert_eq!(config.chunk_size, 8192);
    }

    #[test]
    fn strip_prefix_is_normalized() {
        let root = root_dir();
        let cfg = |v: &str| {
            parse(format!(r#"{{"root": "{root}", "strip_prefix": "{v}"}}"#).as_bytes()).unwrap()
        };
        assert_eq!(cfg("/static/").strip_prefix.as_deref(), Some("/static"));
        assert_eq!(cfg("/static").strip_prefix.as_deref(), Some("/static"));
        assert_eq!(cfg("static").strip_prefix.as_deref(), Some("/static"));
        assert_eq!(cfg("/a/b/").strip_prefix.as_deref(), Some("/a/b"));
        // Absent, empty, and "/" are handled distinctly.
        assert_eq!(
            parse(format!(r#"{{"root": "{root}"}}"#).as_bytes())
                .unwrap()
                .strip_prefix,
            None
        );
        assert!(parse(format!(r#"{{"root": "{root}", "strip_prefix": "/"}}"#).as_bytes()).is_err());
    }

    #[test]
    fn rejections() {
        assert!(parse(b"{}").is_err());
        assert!(parse(b"[]").is_err());
        let root = root_dir();
        assert!(parse(format!(r#"{{"root": "{root}", "nope": 1}}"#).as_bytes()).is_err());
        assert!(parse(format!(r#"{{"root": "{root}", "chunk_size": 1}}"#).as_bytes()).is_err());
        assert!(
            parse(format!(r#"{{"root": "{root}", "directory": "bogus"}}"#).as_bytes()).is_err()
        );
        assert!(parse(br#"{"root": "/definitely/not/a/real/dir"}"#).is_err());
    }
}
