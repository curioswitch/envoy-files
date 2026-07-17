//! Resolution of an HTTP request path into a safe, root-relative filesystem
//! path.

/// Policy governing which request paths are considered acceptable.
pub struct PathPolicy {
    /// When false, any path segment beginning with `.` (other than the
    /// special `.`/`..` segments, which are always rejected) is
    /// rejected.
    pub serve_dotfiles: bool,
    /// Maximum length, in bytes, of the raw path (including query/fragment
    /// stripped by the caller... actually measured on the untouched input,
    /// before the `?`/`#` split, so callers get a stable limit regardless of
    /// query string size).
    pub max_path_len: usize,
}

/// Outcome of resolving a raw request path.
pub enum ValidatedPath {
    /// A safe, root-relative path with no leading slash, using forward
    /// slashes, and free of `.`/`..` segments. The caller joins this under
    /// the configured document root.
    Ok {
        relative: String,
        trailing_slash: bool,
    },
    Reject(RejectReason),
}

/// Why a request path was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    BadEncoding,
    Traversal,
    Dotfile,
    TooLong,
    BadCharacter,
}

/// Resolves a raw HTTP request-target path (the `:path` pseudo-header,
/// including any query/fragment) into either a safe relative filesystem path
/// or a rejection reason.
pub fn resolve_request_path(raw_path: &str, policy: &PathPolicy) -> ValidatedPath {
    if raw_path.len() > policy.max_path_len {
        return ValidatedPath::Reject(RejectReason::TooLong);
    }

    let path_only = strip_query_and_fragment(raw_path);
    let trailing_slash = path_only.ends_with('/');

    // Splitting on '/' before percent-decoding is the key safety property:
    // a raw "%2F" stays inside whatever segment it appeared in rather than
    // introducing a new path separator, so it can never be used to smuggle a
    // ".." or absolute path past the segment-level checks below.
    let mut resolved_segments: Vec<String> = Vec::new();
    for raw_segment in path_only.split('/') {
        if raw_segment.is_empty() {
            // Drops both the leading empty segment (from the initial '/')
            // and any doubled slashes ("//").
            continue;
        }

        let decoded = match percent_decode_segment(raw_segment) {
            Ok(decoded) => decoded,
            Err(reason) => return ValidatedPath::Reject(reason),
        };

        // "." and ".." are special segments handled here, before the
        // general character/name checks below (which would otherwise
        // reject them as ending in '.').
        if decoded == "." {
            continue;
        }
        if decoded == ".." {
            return ValidatedPath::Reject(RejectReason::Traversal);
        }

        if let Some(reason) = reject_reason_for_segment(&decoded, policy) {
            return ValidatedPath::Reject(reason);
        }

        resolved_segments.push(decoded);
    }

    ValidatedPath::Ok {
        relative: resolved_segments.join("/"),
        trailing_slash,
    }
}

/// Strips everything from the first `?` or `#`, whichever comes first.
fn strip_query_and_fragment(raw_path: &str) -> &str {
    let cut = raw_path
        .char_indices()
        .find(|(_, c)| *c == '?' || *c == '#')
        .map(|(i, _)| i);
    match cut {
        Some(i) => &raw_path[..i],
        None => raw_path,
    }
}

/// Percent-decodes a single path segment (no '/' may appear in the input,
/// since the caller splits on '/' first). Rejects any decoded '/' — an
/// encoded slash must not smuggle a new path separator into the segment —
/// and rejects bytes that don't form valid UTF-8. A malformed escape (a `%`
/// not followed by two hex digits) is passed through literally.
fn percent_decode_segment(segment: &str) -> Result<String, RejectReason> {
    let decoded = percent_encoding::percent_decode(segment.as_bytes())
        .decode_utf8()
        .map_err(|_| RejectReason::BadEncoding)?;

    if decoded.contains('/') {
        return Err(RejectReason::Traversal);
    }

    Ok(decoded.into_owned())
}

/// Checks a single decoded, non-empty, non-`.`/`..` segment against all of
/// the character- and name-based rejection rules. `.`/`..` must be handled
/// by the caller before invoking this.
fn reject_reason_for_segment(segment: &str, policy: &PathPolicy) -> Option<RejectReason> {
    for c in segment.chars() {
        if c == '\\' {
            return Some(RejectReason::BadCharacter);
        }
        if c == ':' {
            return Some(RejectReason::BadCharacter);
        }
        if (c as u32) < 0x20 {
            return Some(RejectReason::BadCharacter);
        }
    }

    if segment.ends_with('.') || segment.ends_with(' ') {
        return Some(RejectReason::BadCharacter);
    }

    if !policy.serve_dotfiles && segment.starts_with('.') {
        return Some(RejectReason::Dotfile);
    }

    if is_windows_reserved_name(segment) {
        return Some(RejectReason::BadCharacter);
    }

    None
}

/// Reports whether `segment`'s basename (the part before the first `.`)
/// case-insensitively matches a reserved Windows device name, regardless of
/// any extension(s) that follow (e.g. "con", "CON.txt", "com1.tar.gz").
fn is_windows_reserved_name(segment: &str) -> bool {
    let basename = match segment.split_once('.') {
        Some((base, _)) => base,
        None => segment,
    };

    const RESERVED: &[&str] = &[
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];

    RESERVED
        .iter()
        .any(|name| name.eq_ignore_ascii_case(basename))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(serve_dotfiles: bool) -> PathPolicy {
        PathPolicy {
            serve_dotfiles,
            max_path_len: 4096,
        }
    }

    fn resolve_ok(raw: &str, policy: &PathPolicy) -> (String, bool) {
        match resolve_request_path(raw, policy) {
            ValidatedPath::Ok {
                relative,
                trailing_slash,
            } => (relative, trailing_slash),
            ValidatedPath::Reject(reason) => {
                panic!("expected Ok for {raw:?}, got Reject({reason:?})")
            }
        }
    }

    fn resolve_reject(raw: &str, policy: &PathPolicy) -> RejectReason {
        match resolve_request_path(raw, policy) {
            ValidatedPath::Ok { relative, .. } => {
                panic!("expected Reject for {raw:?}, got Ok({relative:?})")
            }
            ValidatedPath::Reject(reason) => reason,
        }
    }

    #[test]
    fn root_path_is_empty_with_trailing_slash() {
        let (relative, trailing_slash) = resolve_ok("/", &policy(false));
        assert_eq!(relative, "");
        assert!(trailing_slash);
    }

    #[test]
    fn valid_simple_path() {
        let (relative, trailing_slash) = resolve_ok("/valid/path.txt", &policy(false));
        assert_eq!(relative, "valid/path.txt");
        assert!(!trailing_slash);
    }

    #[test]
    fn double_slash_collapses() {
        let (relative, _) = resolve_ok("/a//b", &policy(false));
        assert_eq!(relative, "a/b");
    }

    #[test]
    fn dot_segment_is_dropped() {
        let (relative, _) = resolve_ok("/./a", &policy(false));
        assert_eq!(relative, "a");
    }

    #[test]
    fn trailing_slash_preserved() {
        let (relative, trailing_slash) = resolve_ok("/a/", &policy(false));
        assert_eq!(relative, "a");
        assert!(trailing_slash);
    }

    #[test]
    fn query_string_is_stripped() {
        let (relative, trailing_slash) = resolve_ok("/a/b?x=1&y=2", &policy(false));
        assert_eq!(relative, "a/b");
        assert!(!trailing_slash);
    }

    #[test]
    fn fragment_is_stripped() {
        let (relative, _) = resolve_ok("/a/b#section", &policy(false));
        assert_eq!(relative, "a/b");
    }

    #[test]
    fn percent_decoding_inside_segment() {
        let (relative, _) = resolve_ok("/hello%20world.txt", &policy(false));
        assert_eq!(relative, "hello world.txt");
    }

    #[test]
    fn dotfiles_allowed_when_policy_enables() {
        let (relative, _) = resolve_ok("/.well-known/foo", &policy(true));
        assert_eq!(relative, ".well-known/foo");
    }

    #[test]
    fn dotfiles_rejected_by_default() {
        assert_eq!(
            resolve_reject("/.well-known/foo", &policy(false)),
            RejectReason::Dotfile
        );
    }

    #[test]
    fn simple_traversal_rejected() {
        assert_eq!(
            resolve_reject("/../etc/passwd", &policy(false)),
            RejectReason::Traversal
        );
    }

    #[test]
    fn nested_traversal_rejected() {
        assert_eq!(
            resolve_reject("/a/../../b", &policy(false)),
            RejectReason::Traversal
        );
    }

    #[test]
    fn encoded_traversal_lowercase_rejected() {
        assert_eq!(
            resolve_reject("/a/%2e%2e/b", &policy(false)),
            RejectReason::Traversal
        );
    }

    #[test]
    fn encoded_traversal_uppercase_rejected() {
        assert_eq!(
            resolve_reject("/a/%2E%2E/b", &policy(false)),
            RejectReason::Traversal
        );
    }

    #[test]
    fn fully_encoded_traversal_rejected() {
        assert_eq!(
            resolve_reject("/%2e%2e%2f", &policy(false)),
            RejectReason::Traversal
        );
    }

    #[test]
    fn encoded_slash_before_traversal_rejected() {
        // "..%2fb" decodes within a single segment to "../b", which
        // contains a decoded '/' and is rejected as Traversal before the
        // ".." check even runs.
        assert_eq!(
            resolve_reject("/a/..%2fb", &policy(false)),
            RejectReason::Traversal
        );
    }

    #[test]
    fn encoded_slash_prefix_rejected() {
        assert_eq!(
            resolve_reject("/a%2f..%2fb", &policy(false)),
            RejectReason::Traversal
        );
    }

    #[test]
    fn windows_reserved_name_bare() {
        assert_eq!(
            resolve_reject("/a/con.txt", &policy(false)),
            RejectReason::BadCharacter
        );
        assert_eq!(
            resolve_reject("/a/aux", &policy(false)),
            RejectReason::BadCharacter
        );
        assert_eq!(
            resolve_reject("/lpt9.tar.gz", &policy(false)),
            RejectReason::BadCharacter
        );
        assert_eq!(
            resolve_reject("/CON", &policy(false)),
            RejectReason::BadCharacter
        );
        assert_eq!(
            resolve_reject("/com1.tar.gz", &policy(false)),
            RejectReason::BadCharacter
        );
    }

    #[test]
    fn similar_but_not_reserved_name_is_fine() {
        let (relative, _) = resolve_ok("/console.txt", &policy(false));
        assert_eq!(relative, "console.txt");
    }

    #[test]
    fn backslash_rejected() {
        assert_eq!(
            resolve_reject("/a\\b", &policy(false)),
            RejectReason::BadCharacter
        );
    }

    #[test]
    fn encoded_backslash_rejected() {
        assert_eq!(
            resolve_reject("/a%5cb", &policy(false)),
            RejectReason::BadCharacter
        );
    }

    #[test]
    fn ntfs_ads_colon_rejected() {
        assert_eq!(
            resolve_reject("/file.txt:$DATA", &policy(false)),
            RejectReason::BadCharacter
        );
    }

    #[test]
    fn dotfile_deep_path_rejected() {
        assert_eq!(
            resolve_reject("/a/.git/config", &policy(false)),
            RejectReason::Dotfile
        );
    }

    #[test]
    fn nul_byte_rejected() {
        assert_eq!(
            resolve_reject("/%00", &policy(false)),
            RejectReason::BadCharacter
        );
        assert_eq!(
            resolve_reject("/a%00b", &policy(false)),
            RejectReason::BadCharacter
        );
    }

    #[test]
    fn too_long_path_rejected() {
        let long_path = format!("/{}", "a".repeat(5000));
        assert_eq!(
            resolve_reject(&long_path, &policy(false)),
            RejectReason::TooLong
        );
    }

    #[test]
    fn malformed_escape_passes_through_literally() {
        // A `%` not followed by two hex digits is not a valid escape; the
        // `percent-encoding` crate leaves it literal, so it becomes an
        // ordinary (nonexistent) filename rather than a hard rejection.
        assert_eq!(resolve_ok("/%zz", &policy(false)).0, "%zz");
        assert_eq!(resolve_ok("/%2", &policy(false)).0, "%2");
        assert_eq!(resolve_ok("/%", &policy(false)).0, "%");
    }

    #[test]
    fn segment_ending_in_dot_rejected() {
        assert_eq!(
            resolve_reject("/foo./bar", &policy(false)),
            RejectReason::BadCharacter
        );
    }

    #[test]
    fn segment_ending_in_space_rejected() {
        assert_eq!(
            resolve_reject("/foo /bar", &policy(false)),
            RejectReason::BadCharacter
        );
    }

    #[test]
    fn control_char_rejected() {
        assert_eq!(
            resolve_reject("/a%01b", &policy(false)),
            RejectReason::BadCharacter
        );
    }

    #[test]
    fn single_dot_segment_is_not_treated_as_dotfile() {
        // "." is dropped entirely before the dotfile check even runs.
        let (relative, _) = resolve_ok("/./valid.txt", &policy(false));
        assert_eq!(relative, "valid.txt");
    }

    #[test]
    fn double_dot_alone_is_traversal_even_with_dotfiles_allowed() {
        assert_eq!(
            resolve_reject("/..", &policy(true)),
            RejectReason::Traversal
        );
    }

    #[test]
    fn invalid_utf8_after_decode_rejected() {
        // %FF is not valid standalone UTF-8.
        assert_eq!(
            resolve_reject("/%ff", &policy(false)),
            RejectReason::BadEncoding
        );
    }
}
