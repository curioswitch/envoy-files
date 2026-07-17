//! Parsing and planning of HTTP `Range` requests (RFC 9110 §14), including
//! `multipart/byteranges` assembly for multi-range responses.

/// An inclusive byte range, as used on the wire (`Content-Range: bytes
/// start-end/total`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    pub start: u64,
    pub end: u64,
}

/// The result of planning a `Range` request against a resource of a known
/// length.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RangeOutcome {
    /// Serve the whole resource with a 200 response: either there was no
    /// usable range request, or the server chose to fall back rather than
    /// serve a constrained multi-range response.
    Full,
    Single(ByteRange),
    Multi(Vec<ByteRange>),
    /// None of the requested ranges could be satisfied; the caller should
    /// respond 416 with `Content-Range: bytes */total`.
    Unsatisfiable,
}

/// Server-side limits on how much of a multi-range request will be honored.
pub struct RangePolicy {
    pub allow_multi: bool,
    /// Falls back to `Full` when more valid ranges than this are
    /// requested, to bound the amplification a client can trigger with a
    /// pathological `Range` header.
    pub max_parts: usize,
}

/// Plans how to serve `range_header` (the raw `Range:` header value,
/// expected to look like `"bytes=0-499"`) against a resource of
/// `total_len` bytes.
pub fn plan_ranges(range_header: &str, total_len: u64, policy: &RangePolicy) -> RangeOutcome {
    let Some(specs_str) = range_header.strip_prefix("bytes=") else {
        return RangeOutcome::Full;
    };

    let mut valid_ranges: Vec<ByteRange> = Vec::new();
    let mut saw_any_spec = false;

    for raw_spec in specs_str.split(',') {
        let trimmed = raw_spec.trim();
        if trimmed.is_empty() {
            continue;
        }
        saw_any_spec = true;

        match parse_one_spec(trimmed, total_len) {
            None => return RangeOutcome::Full, // syntactically invalid: ignore the whole header
            Some(SpecResult::Unsatisfiable) => {}
            Some(SpecResult::Range(range)) => valid_ranges.push(range),
        }
    }

    if !saw_any_spec {
        return RangeOutcome::Full;
    }

    if valid_ranges.is_empty() {
        return RangeOutcome::Unsatisfiable;
    }

    if valid_ranges.len() == 1 {
        return RangeOutcome::Single(valid_ranges[0]);
    }

    if !policy.allow_multi || valid_ranges.len() > policy.max_parts {
        return RangeOutcome::Full;
    }

    // Require strictly ascending, non-overlapping ranges (in the order
    // requested, with no coalescing): this keeps multi-range responses
    // simple to stream and avoids the amplification that overlapping
    // ranges could otherwise cause.
    if !is_ascending_non_overlapping(&valid_ranges) {
        return RangeOutcome::Full;
    }

    RangeOutcome::Multi(valid_ranges)
}

enum SpecResult {
    Unsatisfiable,
    Range(ByteRange),
}

/// Parses one comma-separated range-spec. Returns `None` when the spec is
/// syntactically invalid (which per RFC 9110 means the entire `Range`
/// header must be ignored), `Some(Unsatisfiable)` when it is well-formed
/// but names no available bytes, or `Some(Range(_))` otherwise.
fn parse_one_spec(spec: &str, total_len: u64) -> Option<SpecResult> {
    if let Some(suffix_digits) = spec.strip_prefix('-') {
        return parse_suffix_spec(suffix_digits, total_len);
    }

    let (first_digits, last_digits) = spec.split_once('-')?;
    if first_digits.is_empty() || !is_all_ascii_digits(first_digits) {
        return None;
    }
    let first_byte_pos: u64 = first_digits.parse().ok()?;

    if last_digits.is_empty() {
        return Some(open_ended_spec(first_byte_pos, total_len));
    }

    if !is_all_ascii_digits(last_digits) {
        return None;
    }
    let last_byte_pos: u64 = last_digits.parse().ok()?;

    if last_byte_pos < first_byte_pos {
        return Some(SpecResult::Unsatisfiable);
    }
    if total_len == 0 || first_byte_pos >= total_len {
        return Some(SpecResult::Unsatisfiable);
    }

    Some(SpecResult::Range(ByteRange {
        start: first_byte_pos,
        end: last_byte_pos.min(total_len - 1),
    }))
}

fn parse_suffix_spec(suffix_digits: &str, total_len: u64) -> Option<SpecResult> {
    if suffix_digits.is_empty() || !is_all_ascii_digits(suffix_digits) {
        return None;
    }
    let suffix_length: u64 = suffix_digits.parse().ok()?;

    if suffix_length == 0 || total_len == 0 {
        return Some(SpecResult::Unsatisfiable);
    }
    if suffix_length >= total_len {
        return Some(SpecResult::Range(ByteRange {
            start: 0,
            end: total_len - 1,
        }));
    }
    Some(SpecResult::Range(ByteRange {
        start: total_len - suffix_length,
        end: total_len - 1,
    }))
}

fn open_ended_spec(first_byte_pos: u64, total_len: u64) -> SpecResult {
    if total_len == 0 || first_byte_pos >= total_len {
        return SpecResult::Unsatisfiable;
    }
    SpecResult::Range(ByteRange {
        start: first_byte_pos,
        end: total_len - 1,
    })
}

fn is_all_ascii_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

fn is_ascending_non_overlapping(ranges: &[ByteRange]) -> bool {
    ranges.windows(2).all(|pair| pair[1].start > pair[0].end)
}

/// Formats the `Content-Range` header value for a satisfiable range.
pub fn content_range(range: &ByteRange, total: u64) -> String {
    format!("bytes {}-{}/{}", range.start, range.end, total)
}

/// Formats the `Content-Range` header value for a 416 response.
pub fn content_range_unsatisfied(total: u64) -> String {
    format!("bytes */{total}")
}

/// One part of a `multipart/byteranges` body: the range it carries, and the
/// exact header bytes (including boundary and CRLF framing) that precede
/// the range's raw file bytes.
pub struct MultipartPart {
    pub range: ByteRange,
    pub header: Vec<u8>,
}

/// Builds the per-part headers, the closing boundary, and the exact total
/// `Content-Length` for a `multipart/byteranges` body covering `ranges` of
/// a `total`-byte resource with content type `content_type` and boundary
/// `boundary`.
///
/// Framing follows RFC 2046 §5.1.1: the first boundary line has no leading
/// CRLF (`"--B\r\n..."`), every subsequent one does
/// (`"\r\n--B\r\n..."`), and the body ends with `"\r\n--B--\r\n"`.
pub fn multipart_parts(
    ranges: &[ByteRange],
    content_type: &str,
    total: u64,
    boundary: &str,
) -> (Vec<MultipartPart>, Vec<u8>, u64) {
    let mut parts = Vec::with_capacity(ranges.len());
    let mut content_length: u64 = 0;

    for (index, range) in ranges.iter().enumerate() {
        let mut header = Vec::new();
        if index > 0 {
            header.extend_from_slice(b"\r\n");
        }
        header.extend_from_slice(b"--");
        header.extend_from_slice(boundary.as_bytes());
        header.extend_from_slice(b"\r\n");
        header.extend_from_slice(b"Content-Type: ");
        header.extend_from_slice(content_type.as_bytes());
        header.extend_from_slice(b"\r\n");
        header.extend_from_slice(
            format!("Content-Range: {}\r\n\r\n", content_range(range, total)).as_bytes(),
        );

        content_length += header.len() as u64;
        content_length += range.end - range.start + 1;

        parts.push(MultipartPart {
            range: *range,
            header,
        });
    }

    let terminator = format!("\r\n--{boundary}--\r\n").into_bytes();
    content_length += terminator.len() as u64;

    (parts, terminator, content_length)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(allow_multi: bool, max_parts: usize) -> RangePolicy {
        RangePolicy {
            allow_multi,
            max_parts,
        }
    }

    fn default_policy() -> RangePolicy {
        policy(true, 50)
    }

    fn single(outcome: RangeOutcome) -> ByteRange {
        match outcome {
            RangeOutcome::Single(range) => range,
            other => panic!("expected Single, got {other:?}"),
        }
    }

    #[test]
    fn full_byte_at_start() {
        let r = single(plan_ranges("bytes=0-0", 10_000, &default_policy()));
        assert_eq!(r, ByteRange { start: 0, end: 0 });
    }

    #[test]
    fn simple_prefix_range() {
        let r = single(plan_ranges("bytes=0-499", 10_000, &default_policy()));
        assert_eq!(r, ByteRange { start: 0, end: 499 });
    }

    #[test]
    fn simple_middle_range() {
        let r = single(plan_ranges("bytes=500-999", 10_000, &default_policy()));
        assert_eq!(
            r,
            ByteRange {
                start: 500,
                end: 999
            }
        );
    }

    #[test]
    fn suffix_range() {
        let r = single(plan_ranges("bytes=-500", 10_000, &default_policy()));
        assert_eq!(
            r,
            ByteRange {
                start: 9_500,
                end: 9_999
            }
        );
    }

    #[test]
    fn open_ended_range_near_end() {
        let r = single(plan_ranges("bytes=9500-", 10_000, &default_policy()));
        assert_eq!(
            r,
            ByteRange {
                start: 9_500,
                end: 9_999
            }
        );
    }

    #[test]
    fn open_ended_range_from_start() {
        let r = single(plan_ranges("bytes=0-", 10_000, &default_policy()));
        assert_eq!(
            r,
            ByteRange {
                start: 0,
                end: 9_999
            }
        );
    }

    #[test]
    fn suffix_zero_is_unsatisfiable() {
        assert_eq!(
            plan_ranges("bytes=-0", 10_000, &default_policy()),
            RangeOutcome::Unsatisfiable
        );
    }

    #[test]
    fn start_past_end_is_unsatisfiable() {
        assert_eq!(
            plan_ranges("bytes=99999-", 10_000, &default_policy()),
            RangeOutcome::Unsatisfiable
        );
    }

    #[test]
    fn suffix_larger_than_total_covers_whole_file() {
        let r = single(plan_ranges("bytes=-999999", 10_000, &default_policy()));
        assert_eq!(
            r,
            ByteRange {
                start: 0,
                end: 9_999
            }
        );
    }

    #[test]
    fn ascending_multi_range() {
        match plan_ranges("bytes=0-0,-1", 10_000, &default_policy()) {
            RangeOutcome::Multi(ranges) => {
                assert_eq!(
                    ranges,
                    vec![
                        ByteRange { start: 0, end: 0 },
                        ByteRange {
                            start: 9_999,
                            end: 9_999
                        },
                    ]
                );
            }
            other => panic!("expected Multi, got {other:?}"),
        }
    }

    #[test]
    fn overlapping_multi_range_falls_back_to_full() {
        assert_eq!(
            plan_ranges("bytes=0-10,5-15", 10_000, &default_policy()),
            RangeOutcome::Full
        );
    }

    #[test]
    fn descending_multi_range_falls_back_to_full() {
        assert_eq!(
            plan_ranges("bytes=10-20,0-5", 10_000, &default_policy()),
            RangeOutcome::Full
        );
    }

    #[test]
    fn too_many_parts_falls_back_to_full() {
        let specs: Vec<String> = (0..51).map(|i| format!("{i}-{i}")).collect();
        let header = format!("bytes={}", specs.join(","));
        assert_eq!(
            plan_ranges(&header, 10_000, &default_policy()),
            RangeOutcome::Full
        );
    }

    #[test]
    fn exactly_max_parts_is_multi() {
        let specs: Vec<String> = (0..50).map(|i| format!("{i}-{i}")).collect();
        let header = format!("bytes={}", specs.join(","));
        match plan_ranges(&header, 10_000, &default_policy()) {
            RangeOutcome::Multi(ranges) => assert_eq!(ranges.len(), 50),
            other => panic!("expected Multi, got {other:?}"),
        }
    }

    #[test]
    fn multi_disallowed_by_policy_falls_back_to_full() {
        assert_eq!(
            plan_ranges("bytes=0-0,-1", 10_000, &policy(false, 50)),
            RangeOutcome::Full
        );
    }

    #[test]
    fn malformed_syntax_falls_back_to_full() {
        assert_eq!(
            plan_ranges("bytes=abc", 10_000, &default_policy()),
            RangeOutcome::Full
        );
    }

    #[test]
    fn wrong_unit_falls_back_to_full() {
        assert_eq!(
            plan_ranges("octets=0-1", 10_000, &default_policy()),
            RangeOutcome::Full
        );
    }

    #[test]
    fn empty_specs_falls_back_to_full() {
        assert_eq!(
            plan_ranges("bytes=", 10_000, &default_policy()),
            RangeOutcome::Full
        );
    }

    #[test]
    fn whitespace_after_comma_is_tolerated() {
        let r = single(plan_ranges("bytes=0-0", 10_000, &default_policy()));
        assert_eq!(r, ByteRange { start: 0, end: 0 });
        match plan_ranges("bytes=0-0, 9999-9999", 10_000, &default_policy()) {
            RangeOutcome::Multi(ranges) => assert_eq!(ranges.len(), 2),
            other => panic!("expected Multi, got {other:?}"),
        }
    }

    #[test]
    fn zero_total_length_is_unsatisfiable() {
        assert_eq!(
            plan_ranges("bytes=0-0", 0, &default_policy()),
            RangeOutcome::Unsatisfiable
        );
        assert_eq!(
            plan_ranges("bytes=-1", 0, &default_policy()),
            RangeOutcome::Unsatisfiable
        );
        assert_eq!(
            plan_ranges("bytes=0-", 0, &default_policy()),
            RangeOutcome::Unsatisfiable
        );
    }

    #[test]
    fn end_less_than_start_is_unsatisfiable() {
        assert_eq!(
            plan_ranges("bytes=500-100", 10_000, &default_policy()),
            RangeOutcome::Unsatisfiable
        );
    }

    #[test]
    fn end_beyond_total_is_clamped() {
        let r = single(plan_ranges("bytes=0-999999", 10_000, &default_policy()));
        assert_eq!(
            r,
            ByteRange {
                start: 0,
                end: 9_999
            }
        );
    }

    #[test]
    fn content_range_header_format() {
        assert_eq!(
            content_range(&ByteRange { start: 0, end: 499 }, 10_000),
            "bytes 0-499/10000"
        );
    }

    #[test]
    fn content_range_unsatisfied_format() {
        assert_eq!(content_range_unsatisfied(10_000), "bytes */10000");
    }

    #[test]
    fn multipart_single_part_framing_and_length() {
        let ranges = vec![ByteRange { start: 0, end: 4 }];
        let (parts, terminator, content_length) =
            multipart_parts(&ranges, "text/plain", 100, "BOUNDARY");

        assert_eq!(parts.len(), 1);
        let expected_header =
            b"--BOUNDARY\r\nContent-Type: text/plain\r\nContent-Range: bytes 0-4/100\r\n\r\n";
        assert_eq!(parts[0].header, expected_header);
        assert_eq!(terminator, b"\r\n--BOUNDARY--\r\n");

        let mut assembled = Vec::new();
        for part in &parts {
            assembled.extend_from_slice(&part.header);
            assembled.extend(std::iter::repeat_n(b'x', 5)); // 5 bytes of body: 0..=4
        }
        assembled.extend_from_slice(&terminator);
        assert_eq!(assembled.len() as u64, content_length);
    }

    #[test]
    fn multipart_multi_part_content_length_matches_assembled_bytes() {
        let file: Vec<u8> = (0..=255u16).map(|b| b as u8).collect(); // 256 bytes
        let ranges = vec![
            ByteRange { start: 0, end: 9 }, // 10 bytes
            ByteRange {
                start: 100,
                end: 149,
            }, // 50 bytes
            ByteRange {
                start: 200,
                end: 255,
            }, // 56 bytes
        ];
        let (parts, terminator, content_length) = multipart_parts(
            &ranges,
            "application/octet-stream",
            file.len() as u64,
            "B123",
        );

        assert_eq!(parts.len(), 3);
        // First part has no leading CRLF; subsequent parts do.
        assert!(parts[0].header.starts_with(b"--B123\r\n"));
        assert!(parts[1].header.starts_with(b"\r\n--B123\r\n"));
        assert!(parts[2].header.starts_with(b"\r\n--B123\r\n"));

        let mut assembled = Vec::new();
        for part in &parts {
            assembled.extend_from_slice(&part.header);
            let start = part.range.start as usize;
            let end = part.range.end as usize;
            assembled.extend_from_slice(&file[start..=end]);
        }
        assembled.extend_from_slice(&terminator);

        assert_eq!(assembled.len() as u64, content_length);
    }
}
