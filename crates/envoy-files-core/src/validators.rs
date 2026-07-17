//! Strong validators (ETag, Last-Modified) and RFC 9110 conditional-request
//! evaluation (If-Match, If-None-Match, If-Modified-Since,
//! If-Unmodified-Since, If-Range).

use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use headers::{
    ETag, Header, IfMatch, IfModifiedSince, IfNoneMatch, IfRange, IfUnmodifiedSince, LastModified,
};
use http::HeaderValue;

/// The validators computed for a served resource.
pub struct Validators {
    /// Includes the surrounding double quotes, e.g. `"\"1a2b-3c4d.0\""`.
    /// Always a strong validator (never `W/`-prefixed).
    pub etag: String,
    pub last_modified_unix: i64,
}

/// Builds a strong ETag from a file's size and modification time. The
/// format is `"{size:x}-{secs:x}.{nanos:x}"`, quoted.
pub fn make_etag(size: u64, mtime_unix: i64, mtime_nanos: u32) -> String {
    // mtime_unix is treated as its bit pattern rather than rejecting
    // negative values, since pre-1970 mtimes are rare but not impossible on
    // some filesystems and this only needs to be a stable fingerprint, not
    // a meaningful number on its own.
    format!("\"{:x}-{:x}.{:x}\"", size, mtime_unix as u64, mtime_nanos)
}

/// Formats a Unix timestamp as an HTTP IMF-fixdate, e.g.
/// `"Sun, 06 Nov 1994 08:49:37 GMT"`. Sub-second precision is discarded.
pub fn format_http_date(unix: i64) -> String {
    httpdate::fmt_http_date(system_time(unix))
}

/// Conditional-request headers, as raw header values.
pub struct ConditionalHeaders<'a> {
    pub if_none_match: Option<&'a str>,
    pub if_modified_since: Option<&'a str>,
    pub if_match: Option<&'a str>,
    pub if_unmodified_since: Option<&'a str>,
    pub if_range: Option<&'a str>,
}

/// Result of evaluating the applicable conditional-request headers against
/// the current validators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Condition {
    Proceed,
    NotModified,
    PreconditionFailed,
}

/// Evaluates conditional headers in RFC 9110 §13.2.2 precedence order:
/// If-Match, then If-Unmodified-Since, then If-None-Match, then
/// If-Modified-Since. An unparseable conditional header is ignored, as its
/// typed decode simply fails.
pub fn evaluate_conditionals(headers: &ConditionalHeaders, v: &Validators) -> Condition {
    let etag = resource_etag(v);
    let last_modified = system_time(v.last_modified_unix);

    // If-Match (else If-Unmodified-Since) can fail the precondition (412).
    if let Some(raw) = headers.if_match {
        if let (Some(if_match), Some(tag)) = (decode::<IfMatch>(raw), etag.as_ref())
            && !if_match.precondition_passes(tag)
        {
            return Condition::PreconditionFailed;
        }
    } else if let Some(raw) = headers.if_unmodified_since
        && let Some(if_unmodified_since) = decode::<IfUnmodifiedSince>(raw)
        && !if_unmodified_since.precondition_passes(last_modified)
    {
        return Condition::PreconditionFailed;
    }

    // If-None-Match (else If-Modified-Since) can short-circuit to 304.
    if let Some(raw) = headers.if_none_match {
        if let (Some(if_none_match), Some(tag)) = (decode::<IfNoneMatch>(raw), etag.as_ref())
            && !if_none_match.precondition_passes(tag)
        {
            return Condition::NotModified;
        }
    } else if let Some(raw) = headers.if_modified_since
        && let Some(if_modified_since) = decode::<IfModifiedSince>(raw)
        && !if_modified_since.is_modified(last_modified)
    {
        return Condition::NotModified;
    }

    Condition::Proceed
}

/// Whether an `If-Range` value permits serving a range: true when the
/// resource has not been modified relative to the client's validator. Per
/// RFC 9110 §13.1.5 a weak ETag never satisfies If-Range (the `headers`
/// crate enforces this via strong comparison). An unparseable value serves
/// the full representation.
pub fn if_range_matches(if_range: &str, v: &Validators) -> bool {
    let Some(if_range) = decode::<IfRange>(if_range) else {
        return false;
    };
    let etag = resource_etag(v);
    let last_modified = LastModified::from(system_time(v.last_modified_unix));
    !if_range.is_modified(etag.as_ref(), Some(&last_modified))
}

/// Decodes a single raw header value into a typed `headers` header, or
/// `None` if the value is not valid for that header.
fn decode<H: Header>(raw: &str) -> Option<H> {
    let value = HeaderValue::from_str(raw).ok()?;
    H::decode(&mut std::iter::once(&value)).ok()
}

/// The resource's ETag as a typed value. Always `Some` for etags produced by
/// [`make_etag`]; `None` only if a caller supplied a malformed string.
fn resource_etag(v: &Validators) -> Option<ETag> {
    ETag::from_str(&v.etag).ok()
}

fn system_time(unix: i64) -> SystemTime {
    if unix >= 0 {
        UNIX_EPOCH + Duration::from_secs(unix as u64)
    } else {
        UNIX_EPOCH - Duration::from_secs(unix.unsigned_abs())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn validators(etag: &str, last_modified_unix: i64) -> Validators {
        Validators {
            etag: etag.to_string(),
            last_modified_unix,
        }
    }

    fn no_conditionals() -> ConditionalHeaders<'static> {
        ConditionalHeaders {
            if_none_match: None,
            if_modified_since: None,
            if_match: None,
            if_unmodified_since: None,
            if_range: None,
        }
    }

    #[test]
    fn make_etag_format() {
        assert_eq!(make_etag(0x64, 0x3e8, 0x1f4), "\"64-3e8.1f4\"");
        assert_eq!(make_etag(0, 0, 0), "\"0-0.0\"");
    }

    #[test]
    fn format_http_date_epoch() {
        assert_eq!(format_http_date(0), "Thu, 01 Jan 1970 00:00:00 GMT");
    }

    #[test]
    fn format_http_date_known() {
        assert_eq!(
            format_http_date(784_111_777),
            "Sun, 06 Nov 1994 08:49:37 GMT"
        );
    }

    #[test]
    fn if_match_star_proceeds() {
        let headers = ConditionalHeaders {
            if_match: Some("*"),
            ..no_conditionals()
        };
        let v = validators("\"abc\"", 1000);
        assert_eq!(evaluate_conditionals(&headers, &v), Condition::Proceed);
    }

    #[test]
    fn if_match_mismatch_fails_precondition() {
        let headers = ConditionalHeaders {
            if_match: Some("\"other\""),
            ..no_conditionals()
        };
        let v = validators("\"abc\"", 1000);
        assert_eq!(
            evaluate_conditionals(&headers, &v),
            Condition::PreconditionFailed
        );
    }

    #[test]
    fn if_match_list_with_match_proceeds() {
        let headers = ConditionalHeaders {
            if_match: Some("\"x\", \"abc\", \"y\""),
            ..no_conditionals()
        };
        let v = validators("\"abc\"", 1000);
        assert_eq!(evaluate_conditionals(&headers, &v), Condition::Proceed);
    }

    #[test]
    fn if_match_uses_strong_comparison_weak_etag_fails() {
        let headers = ConditionalHeaders {
            if_match: Some("W/\"abc\""),
            ..no_conditionals()
        };
        let v = validators("\"abc\"", 1000);
        assert_eq!(
            evaluate_conditionals(&headers, &v),
            Condition::PreconditionFailed
        );
    }

    #[test]
    fn if_unmodified_since_only_checked_without_if_match() {
        let headers = ConditionalHeaders {
            if_unmodified_since: Some("Sun, 06 Nov 1994 08:49:37 GMT"),
            ..no_conditionals()
        };
        let v = validators("\"abc\"", 784_111_778); // modified 1s later
        assert_eq!(
            evaluate_conditionals(&headers, &v),
            Condition::PreconditionFailed
        );
    }

    #[test]
    fn if_unmodified_since_ignored_when_if_match_present() {
        let headers = ConditionalHeaders {
            if_match: Some("*"),
            if_unmodified_since: Some("Sun, 06 Nov 1994 08:49:37 GMT"),
            ..no_conditionals()
        };
        let v = validators("\"abc\"", 784_111_778);
        assert_eq!(evaluate_conditionals(&headers, &v), Condition::Proceed);
    }

    #[test]
    fn if_unmodified_since_not_modified_proceeds() {
        let headers = ConditionalHeaders {
            if_unmodified_since: Some("Sun, 06 Nov 1994 08:49:37 GMT"),
            ..no_conditionals()
        };
        let v = validators("\"abc\"", 784_111_777); // exactly equal: satisfied
        assert_eq!(evaluate_conditionals(&headers, &v), Condition::Proceed);
    }

    #[test]
    fn if_none_match_star_not_modified() {
        let headers = ConditionalHeaders {
            if_none_match: Some("*"),
            ..no_conditionals()
        };
        let v = validators("\"abc\"", 1000);
        assert_eq!(evaluate_conditionals(&headers, &v), Condition::NotModified);
    }

    #[test]
    fn if_none_match_weak_comparison_matches() {
        let headers = ConditionalHeaders {
            if_none_match: Some("W/\"abc\""),
            ..no_conditionals()
        };
        let v = validators("\"abc\"", 1000);
        assert_eq!(evaluate_conditionals(&headers, &v), Condition::NotModified);
    }

    #[test]
    fn if_none_match_list_multiple_etags() {
        let headers = ConditionalHeaders {
            if_none_match: Some("\"x\", \"y\", \"abc\""),
            ..no_conditionals()
        };
        let v = validators("\"abc\"", 1000);
        assert_eq!(evaluate_conditionals(&headers, &v), Condition::NotModified);
    }

    #[test]
    fn if_none_match_no_match_proceeds() {
        let headers = ConditionalHeaders {
            if_none_match: Some("\"x\", \"y\""),
            ..no_conditionals()
        };
        let v = validators("\"abc\"", 1000);
        assert_eq!(evaluate_conditionals(&headers, &v), Condition::Proceed);
    }

    #[test]
    fn if_modified_since_only_checked_without_if_none_match() {
        let headers = ConditionalHeaders {
            if_modified_since: Some("Sun, 06 Nov 1994 08:49:37 GMT"),
            ..no_conditionals()
        };
        let v = validators("\"abc\"", 784_111_777); // exactly at date: not modified
        assert_eq!(evaluate_conditionals(&headers, &v), Condition::NotModified);
    }

    #[test]
    fn if_modified_since_ignored_when_if_none_match_present() {
        let headers = ConditionalHeaders {
            if_none_match: Some("\"other\""), // no match -> proceed
            if_modified_since: Some("Sun, 06 Nov 1994 08:49:37 GMT"),
            ..no_conditionals()
        };
        let v = validators("\"abc\"", 784_111_777); // would be NotModified alone
        assert_eq!(evaluate_conditionals(&headers, &v), Condition::Proceed);
    }

    #[test]
    fn if_modified_since_modified_after_proceeds() {
        let headers = ConditionalHeaders {
            if_modified_since: Some("Sun, 06 Nov 1994 08:49:37 GMT"),
            ..no_conditionals()
        };
        let v = validators("\"abc\"", 784_111_778); // 1s after
        assert_eq!(evaluate_conditionals(&headers, &v), Condition::Proceed);
    }

    #[test]
    fn unparseable_if_modified_since_is_ignored() {
        let headers = ConditionalHeaders {
            if_modified_since: Some("garbage"),
            ..no_conditionals()
        };
        let v = validators("\"abc\"", 1000);
        assert_eq!(evaluate_conditionals(&headers, &v), Condition::Proceed);
    }

    #[test]
    fn no_conditionals_proceeds() {
        let v = validators("\"abc\"", 1000);
        assert_eq!(
            evaluate_conditionals(&no_conditionals(), &v),
            Condition::Proceed
        );
    }

    #[test]
    fn if_range_strong_etag_match() {
        let v = validators("\"abc\"", 1000);
        assert!(if_range_matches("\"abc\"", &v));
    }

    #[test]
    fn if_range_weak_etag_never_matches() {
        let v = validators("\"abc\"", 1000);
        assert!(!if_range_matches("W/\"abc\"", &v));
    }

    #[test]
    fn if_range_etag_mismatch() {
        let v = validators("\"abc\"", 1000);
        assert!(!if_range_matches("\"xyz\"", &v));
    }

    #[test]
    fn if_range_date_not_older_serves_range() {
        // The resource's Last-Modified is not after the client's If-Range
        // date, so the range may be served.
        let v = validators("\"abc\"", 784_111_777);
        assert!(if_range_matches("Sun, 06 Nov 1994 08:49:37 GMT", &v));
    }

    #[test]
    fn if_range_date_modified_since_serves_full() {
        // The resource was modified after the client's If-Range date, so the
        // range must not be served.
        let v = validators("\"abc\"", 784_111_778); // 1s after the If-Range date
        assert!(!if_range_matches("Sun, 06 Nov 1994 08:49:37 GMT", &v));
    }
}
