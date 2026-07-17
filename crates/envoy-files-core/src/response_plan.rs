//! Top-level response planner.

use crate::range::{
    ByteRange, MultipartPart, RangeOutcome, RangePolicy, content_range, content_range_unsatisfied,
    multipart_parts,
};
use crate::validators::{
    Condition, ConditionalHeaders, Validators, evaluate_conditionals, format_http_date,
    if_range_matches, make_etag,
};
use http::{HeaderName, HeaderValue, StatusCode, header};

/// Builds a `HeaderValue` from bytes this crate itself produced. All such
/// values (dates, etags, content-range, content-type) are visible ASCII, so
/// construction never fails in practice.
fn value(bytes: &[u8]) -> HeaderValue {
    HeaderValue::from_bytes(bytes).expect("generated header value is valid")
}

/// Filesystem facts about the resource being served, as already resolved
/// by the caller.
pub struct FileMeta {
    pub size: u64,
    pub mtime_unix: i64,
    pub mtime_nanos: u32,
    pub is_dir: bool,
}

/// What the caller should send as the response body.
pub enum BodyPlan {
    Empty,
    Whole {
        len: u64,
    },
    Segments(Vec<ByteRange>),
    Multipart {
        parts: Vec<MultipartPart>,
        terminator: Vec<u8>,
        content_length: u64,
    },
}

/// Everything about the request that influences the response plan, beyond
/// the resource's own metadata.
pub struct RequestFacts<'a> {
    pub method_head: bool,
    pub conditionals: ConditionalHeaders<'a>,
    pub range_header: Option<&'a str>,
    pub content_type: String,
    pub etag_enabled: bool,
    pub last_modified_enabled: bool,
    pub range_policy: RangePolicy,
    /// Only used when a multi-range request produces a
    /// `multipart/byteranges` body.
    pub boundary: String,
    /// Set when a pre-compressed variant is being served in place of the
    /// identity representation.
    pub content_encoding: Option<&'static str>,
    /// Forces a `Vary: Accept-Encoding` header even when this particular
    /// response isn't itself encoded, e.g. because content negotiation
    /// happened but the identity representation won.
    pub add_vary: bool,
    /// Value for a `Cache-Control` response header, if configured. Emitted on
    /// 200 and 206 responses.
    pub cache_control: Option<&'a str>,
}

/// The fully-decided HTTP response, ready to be written out verbatim.
pub struct ResponsePlan {
    pub status: StatusCode,
    /// Header names (always lowercase, per `HeaderName`) in emission order.
    pub headers: Vec<(HeaderName, HeaderValue)>,
    pub body: BodyPlan,
}

/// Plans the entire response for one request against one already-`stat`-ed
/// file, applying conditional-request and range semantics in RFC 9110
/// order.
pub fn plan_response(facts: &RequestFacts, meta: &FileMeta) -> ResponsePlan {
    let validators = Validators {
        etag: make_etag(meta.size, meta.mtime_unix, meta.mtime_nanos),
        last_modified_unix: meta.mtime_unix,
    };

    // Headers this server doesn't enable are treated as if the client
    // never sent the corresponding conditional at all, rather than being
    // evaluated against a validator the client was never told about.
    let effective_conditionals = ConditionalHeaders {
        if_none_match: if facts.etag_enabled {
            facts.conditionals.if_none_match
        } else {
            None
        },
        if_match: if facts.etag_enabled {
            facts.conditionals.if_match
        } else {
            None
        },
        if_modified_since: if facts.last_modified_enabled {
            facts.conditionals.if_modified_since
        } else {
            None
        },
        if_unmodified_since: if facts.last_modified_enabled {
            facts.conditionals.if_unmodified_since
        } else {
            None
        },
        if_range: facts.conditionals.if_range,
    };

    match evaluate_conditionals(&effective_conditionals, &validators) {
        Condition::PreconditionFailed => {
            return ResponsePlan {
                status: StatusCode::PRECONDITION_FAILED,
                headers: Vec::new(),
                body: BodyPlan::Empty,
            };
        }
        Condition::NotModified => {
            let mut headers = Vec::new();
            push_validator_headers(&mut headers, facts, &validators);
            return ResponsePlan {
                status: StatusCode::NOT_MODIFIED,
                headers,
                body: BodyPlan::Empty,
            };
        }
        Condition::Proceed => {}
    }

    if let Some(range_header) = facts.range_header {
        let if_range_ok = match facts.conditionals.if_range {
            Some(if_range_value) => if_range_matches(if_range_value, &validators),
            None => true,
        };

        if if_range_ok {
            match crate::range::plan_ranges(range_header, meta.size, &facts.range_policy) {
                RangeOutcome::Unsatisfiable => {
                    let headers = vec![(
                        header::CONTENT_RANGE,
                        value(content_range_unsatisfied(meta.size).as_bytes()),
                    )];
                    return ResponsePlan {
                        status: StatusCode::RANGE_NOT_SATISFIABLE,
                        headers,
                        body: BodyPlan::Empty,
                    };
                }
                RangeOutcome::Single(range) => {
                    let mut headers = Vec::new();
                    headers.push((header::CONTENT_TYPE, value(facts.content_type.as_bytes())));
                    headers.push((header::ACCEPT_RANGES, HeaderValue::from_static("bytes")));
                    push_validator_headers(&mut headers, facts, &validators);
                    push_encoding_headers(&mut headers, facts);
                    let range_len = range.end - range.start + 1;
                    headers.push((
                        header::CONTENT_RANGE,
                        value(content_range(&range, meta.size).as_bytes()),
                    ));
                    headers.push((header::CONTENT_LENGTH, HeaderValue::from(range_len)));

                    let body = if facts.method_head {
                        BodyPlan::Empty
                    } else {
                        BodyPlan::Segments(vec![range])
                    };
                    return ResponsePlan {
                        status: StatusCode::PARTIAL_CONTENT,
                        headers,
                        body,
                    };
                }
                RangeOutcome::Multi(ranges) => {
                    let (parts, terminator, content_length) =
                        multipart_parts(&ranges, &facts.content_type, meta.size, &facts.boundary);

                    let mut headers = Vec::new();
                    headers.push((header::ACCEPT_RANGES, HeaderValue::from_static("bytes")));
                    push_validator_headers(&mut headers, facts, &validators);
                    push_encoding_headers(&mut headers, facts);
                    headers.push((
                        header::CONTENT_TYPE,
                        value(
                            format!("multipart/byteranges; boundary={}", facts.boundary).as_bytes(),
                        ),
                    ));
                    headers.push((header::CONTENT_LENGTH, HeaderValue::from(content_length)));

                    let body = if facts.method_head {
                        BodyPlan::Empty
                    } else {
                        BodyPlan::Multipart {
                            parts,
                            terminator,
                            content_length,
                        }
                    };
                    return ResponsePlan {
                        status: StatusCode::PARTIAL_CONTENT,
                        headers,
                        body,
                    };
                }
                RangeOutcome::Full => {
                    // Fall through to the plain 200 response below.
                }
            }
        }
    }

    let mut headers = Vec::new();
    headers.push((header::CONTENT_TYPE, value(facts.content_type.as_bytes())));
    headers.push((header::ACCEPT_RANGES, HeaderValue::from_static("bytes")));
    push_validator_headers(&mut headers, facts, &validators);
    push_encoding_headers(&mut headers, facts);
    headers.push((header::CONTENT_LENGTH, HeaderValue::from(meta.size)));

    let body = if facts.method_head {
        BodyPlan::Empty
    } else {
        BodyPlan::Whole { len: meta.size }
    };

    ResponsePlan {
        status: StatusCode::OK,
        headers,
        body,
    }
}

fn push_validator_headers(
    headers: &mut Vec<(HeaderName, HeaderValue)>,
    facts: &RequestFacts,
    validators: &Validators,
) {
    if facts.etag_enabled {
        headers.push((header::ETAG, value(validators.etag.as_bytes())));
    }
    if facts.last_modified_enabled {
        headers.push((
            header::LAST_MODIFIED,
            value(format_http_date(validators.last_modified_unix).as_bytes()),
        ));
    }
}

/// Pushes content-negotiation headers (content-encoding, vary) and, when
/// configured, cache-control. Called for every cacheable 200/206 response.
fn push_encoding_headers(headers: &mut Vec<(HeaderName, HeaderValue)>, facts: &RequestFacts) {
    if let Some(content_encoding) = facts.content_encoding {
        headers.push((
            header::CONTENT_ENCODING,
            HeaderValue::from_static(content_encoding),
        ));
    }
    if facts.content_encoding.is_some() || facts.add_vary {
        headers.push((header::VARY, HeaderValue::from_static("Accept-Encoding")));
    }
    if let Some(cache_control) = facts.cache_control {
        headers.push((header::CACHE_CONTROL, value(cache_control.as_bytes())));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_conditionals() -> ConditionalHeaders<'static> {
        ConditionalHeaders {
            if_none_match: None,
            if_modified_since: None,
            if_match: None,
            if_unmodified_since: None,
            if_range: None,
        }
    }

    fn base_facts() -> RequestFacts<'static> {
        RequestFacts {
            method_head: false,
            conditionals: no_conditionals(),
            range_header: None,
            content_type: "text/plain; charset=utf-8".to_string(),
            etag_enabled: true,
            last_modified_enabled: true,
            range_policy: RangePolicy {
                allow_multi: true,
                max_parts: 50,
            },
            boundary: "BOUNDARY".to_string(),
            content_encoding: None,
            add_vary: false,
            cache_control: None,
        }
    }

    fn base_meta() -> FileMeta {
        FileMeta {
            size: 1000,
            mtime_unix: 784_111_777,
            mtime_nanos: 0,
            is_dir: false,
        }
    }

    fn header<'a>(plan: &'a ResponsePlan, name: &str) -> Option<&'a [u8]> {
        plan.headers
            .iter()
            .find(|(n, _)| n.as_str() == name)
            .map(|(_, v)| v.as_bytes())
    }

    #[test]
    fn plain_get_returns_200_with_whole_body() {
        let plan = plan_response(&base_facts(), &base_meta());
        assert_eq!(plan.status, StatusCode::OK);
        assert_eq!(header(&plan, "content-length"), Some(b"1000".as_slice()));
        assert_eq!(
            header(&plan, "content-type"),
            Some(b"text/plain; charset=utf-8".as_slice())
        );
        assert_eq!(header(&plan, "accept-ranges"), Some(b"bytes".as_slice()));
        assert!(header(&plan, "etag").is_some());
        assert!(header(&plan, "last-modified").is_some());
        match plan.body {
            BodyPlan::Whole { len } => assert_eq!(len, 1000),
            _ => panic!("expected Whole body"),
        }
    }

    #[test]
    fn head_request_has_empty_body_but_same_headers() {
        let mut facts = base_facts();
        facts.method_head = true;
        let plan = plan_response(&facts, &base_meta());
        assert_eq!(plan.status, StatusCode::OK);
        assert_eq!(header(&plan, "content-length"), Some(b"1000".as_slice()));
        assert!(matches!(plan.body, BodyPlan::Empty));
    }

    #[test]
    fn content_encoding_adds_vary_and_content_encoding_headers() {
        let mut facts = base_facts();
        facts.content_encoding = Some("gzip");
        let plan = plan_response(&facts, &base_meta());
        assert_eq!(header(&plan, "content-encoding"), Some(b"gzip".as_slice()));
        assert_eq!(header(&plan, "vary"), Some(b"Accept-Encoding".as_slice()));
    }

    #[test]
    fn add_vary_flag_without_encoding_still_adds_vary() {
        let mut facts = base_facts();
        facts.add_vary = true;
        let plan = plan_response(&facts, &base_meta());
        assert!(header(&plan, "content-encoding").is_none());
        assert_eq!(header(&plan, "vary"), Some(b"Accept-Encoding".as_slice()));
    }

    #[test]
    fn no_encoding_and_no_add_vary_omits_vary() {
        let plan = plan_response(&base_facts(), &base_meta());
        assert!(header(&plan, "vary").is_none());
    }

    #[test]
    fn if_none_match_star_yields_304_with_validators() {
        let mut facts = base_facts();
        facts.conditionals.if_none_match = Some("*");
        let plan = plan_response(&facts, &base_meta());
        assert_eq!(plan.status, StatusCode::NOT_MODIFIED);
        assert!(header(&plan, "etag").is_some());
        assert!(header(&plan, "last-modified").is_some());
        assert!(matches!(plan.body, BodyPlan::Empty));
    }

    #[test]
    fn if_match_mismatch_yields_412() {
        let mut facts = base_facts();
        facts.conditionals.if_match = Some("\"nonexistent\"");
        let plan = plan_response(&facts, &base_meta());
        assert_eq!(plan.status, StatusCode::PRECONDITION_FAILED);
        assert!(matches!(plan.body, BodyPlan::Empty));
    }

    #[test]
    fn disabled_etag_ignores_if_none_match_header() {
        let mut facts = base_facts();
        facts.etag_enabled = false;
        facts.conditionals.if_none_match = Some("*"); // would be 304 if etag were enabled
        let plan = plan_response(&facts, &base_meta());
        assert_eq!(plan.status, StatusCode::OK);
        assert!(header(&plan, "etag").is_none());
    }

    #[test]
    fn disabled_last_modified_omits_header_and_ignores_conditional() {
        let mut facts = base_facts();
        facts.last_modified_enabled = false;
        let formatted = format_http_date(784_111_777);
        facts.conditionals.if_modified_since = Some(&formatted);
        let plan = plan_response(&facts, &base_meta());
        assert_eq!(plan.status, StatusCode::OK);
        assert!(header(&plan, "last-modified").is_none());
    }

    #[test]
    fn single_range_yields_206_with_content_range() {
        let mut facts = base_facts();
        facts.range_header = Some("bytes=0-499");
        let plan = plan_response(&facts, &base_meta());
        assert_eq!(plan.status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            header(&plan, "content-range"),
            Some(b"bytes 0-499/1000".as_slice())
        );
        assert_eq!(header(&plan, "content-length"), Some(b"500".as_slice()));
        match plan.body {
            BodyPlan::Segments(ranges) => {
                assert_eq!(ranges, vec![ByteRange { start: 0, end: 499 }]);
            }
            _ => panic!("expected Segments body"),
        }
    }

    #[test]
    fn head_with_range_has_empty_body_but_range_headers() {
        let mut facts = base_facts();
        facts.method_head = true;
        facts.range_header = Some("bytes=0-499");
        let plan = plan_response(&facts, &base_meta());
        assert_eq!(plan.status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(header(&plan, "content-length"), Some(b"500".as_slice()));
        assert!(matches!(plan.body, BodyPlan::Empty));
    }

    #[test]
    fn unsatisfiable_range_yields_416() {
        let mut facts = base_facts();
        facts.range_header = Some("bytes=5000-6000");
        let plan = plan_response(&facts, &base_meta());
        assert_eq!(plan.status, StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(
            header(&plan, "content-range"),
            Some(b"bytes */1000".as_slice())
        );
        assert!(matches!(plan.body, BodyPlan::Empty));
    }

    #[test]
    fn multi_range_yields_206_multipart() {
        let mut facts = base_facts();
        facts.range_header = Some("bytes=0-9,500-509");
        let plan = plan_response(&facts, &base_meta());
        assert_eq!(plan.status, StatusCode::PARTIAL_CONTENT);
        let content_type = header(&plan, "content-type").unwrap();
        assert!(
            String::from_utf8_lossy(content_type)
                .starts_with("multipart/byteranges; boundary=BOUNDARY")
        );
        match plan.body {
            BodyPlan::Multipart {
                ref parts,
                content_length,
                ..
            } => {
                assert_eq!(parts.len(), 2);
                let content_length_header = header(&plan, "content-length").unwrap();
                assert_eq!(
                    String::from_utf8_lossy(content_length_header),
                    content_length.to_string()
                );
            }
            _ => panic!("expected Multipart body"),
        }
    }

    #[test]
    fn range_falls_back_to_200_when_unparseable() {
        let mut facts = base_facts();
        facts.range_header = Some("bytes=abc");
        let plan = plan_response(&facts, &base_meta());
        assert_eq!(plan.status, StatusCode::OK);
        match plan.body {
            BodyPlan::Whole { len } => assert_eq!(len, 1000),
            _ => panic!("expected Whole body"),
        }
    }

    #[test]
    fn if_range_mismatch_falls_back_to_full_200() {
        let mut facts = base_facts();
        facts.range_header = Some("bytes=0-499");
        facts.conditionals.if_range = Some("\"stale-etag\"");
        let plan = plan_response(&facts, &base_meta());
        assert_eq!(plan.status, StatusCode::OK);
        match plan.body {
            BodyPlan::Whole { len } => assert_eq!(len, 1000),
            _ => panic!("expected Whole body"),
        }
    }

    #[test]
    fn if_range_match_allows_range() {
        let meta = base_meta();
        let etag = make_etag(meta.size, meta.mtime_unix, meta.mtime_nanos);
        let mut facts = base_facts();
        facts.range_header = Some("bytes=0-499");
        facts.conditionals.if_range = Some(Box::leak(etag.into_boxed_str()));
        let plan = plan_response(&facts, &meta);
        assert_eq!(plan.status, StatusCode::PARTIAL_CONTENT);
    }
}
