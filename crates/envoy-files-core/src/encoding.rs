//! Negotiation of pre-compressed asset variants against the client's
//! `Accept-Encoding` header.

/// A pre-compressed encoding the server can serve: the suffix appended to the
/// base filename to locate the variant, and the `Content-Encoding` token
/// (which is also what is matched against `Accept-Encoding`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Encoding {
    pub file_suffix: &'static str,
    pub content_encoding: &'static str,
}

pub const BROTLI: Encoding = Encoding {
    file_suffix: ".br",
    content_encoding: "br",
};
pub const GZIP: Encoding = Encoding {
    file_suffix: ".gz",
    content_encoding: "gzip",
};
pub const ZSTD: Encoding = Encoding {
    file_suffix: ".zst",
    content_encoding: "zstd",
};

/// Returns the encodings from `order` (the server's supported encodings, in
/// server-preference order) that the client's `Accept-Encoding` header
/// allows, sorted by the client's stated preference (`q` value, descending)
/// with ties broken by each encoding's position in `order`.
///
/// A missing `Accept-Encoding` header is treated as compression not supported.
pub fn negotiate_precompressed(accept_encoding: Option<&str>, order: &[Encoding]) -> Vec<Encoding> {
    let Some(header) = accept_encoding else {
        return Vec::new();
    };

    let Some(entries) = parse_accept_encoding(header) else {
        return Vec::new();
    };

    let mut scored: Vec<(f32, usize, Encoding)> = Vec::new();
    for (server_rank, encoding) in order.iter().enumerate() {
        if let Some(q) = acceptance_for(&entries, encoding.content_encoding)
            && q > 0.0
        {
            scored.push((q, server_rank, *encoding));
        }
    }

    // Sort by client q descending, then by server preference order
    // ascending.
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));

    scored
        .into_iter()
        .map(|(_, _, encoding)| encoding)
        .collect()
}

struct AcceptEncodingEntry<'a> {
    name: &'a str,
    q: f32,
}

/// Parses an `Accept-Encoding` header into `(coding, q)` entries. Returns
/// `None` if the header is malformed (e.g. an unparseable `q` value).
fn parse_accept_encoding(header: &str) -> Option<Vec<AcceptEncodingEntry<'_>>> {
    let mut entries = Vec::new();
    for raw_item in header.split(',') {
        let item = raw_item.trim();
        if item.is_empty() {
            continue;
        }

        let mut parts = item.split(';');
        let name = parts.next().unwrap_or("").trim();
        if name.is_empty() {
            return None;
        }

        let mut q: f32 = 1.0;
        for param in parts {
            let param = param.trim();
            let Some(value) = param.strip_prefix("q=") else {
                continue;
            };
            q = value.trim().parse::<f32>().ok()?;
        }

        entries.push(AcceptEncodingEntry { name, q });
    }
    Some(entries)
}

/// Determines the effective `q` value for `coding` per RFC 9110 §12.5.3:
/// an exact (case-insensitive) match wins; otherwise `*` applies; a coding
/// explicitly listed with `q=0` is rejected even if `*` would otherwise
/// allow it. Returns `None` when nothing addresses this coding at all.
fn acceptance_for(entries: &[AcceptEncodingEntry], coding: &str) -> Option<f32> {
    if let Some(entry) = entries.iter().find(|e| e.name.eq_ignore_ascii_case(coding)) {
        return Some(entry.q);
    }
    entries.iter().find(|e| e.name == "*").map(|entry| entry.q)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Encoding; 3] = [BROTLI, GZIP, ZSTD];

    #[test]
    fn no_header_returns_empty() {
        assert_eq!(negotiate_precompressed(None, &ALL), Vec::new());
    }

    #[test]
    fn simple_gzip_only() {
        let result = negotiate_precompressed(Some("gzip"), &ALL);
        assert_eq!(result, vec![GZIP]);
    }

    #[test]
    fn multiple_codings_sorted_by_server_preference_when_q_tied() {
        let result = negotiate_precompressed(Some("gzip, br"), &ALL);
        assert_eq!(result, vec![BROTLI, GZIP,]);
    }

    #[test]
    fn q_values_reorder_preference() {
        let result = negotiate_precompressed(Some("br;q=0.2, gzip;q=0.8"), &ALL);
        assert_eq!(result, vec![GZIP, BROTLI,]);
    }

    #[test]
    fn q_zero_excludes_coding() {
        let result = negotiate_precompressed(Some("gzip;q=0, br"), &ALL);
        assert_eq!(result, vec![BROTLI]);
    }

    #[test]
    fn wildcard_allows_unlisted_coding() {
        let result = negotiate_precompressed(Some("*;q=0.5"), &ALL);
        assert_eq!(result.len(), 3);
    }

    #[test]
    fn wildcard_zero_excludes_all_unlisted() {
        let result = negotiate_precompressed(Some("*;q=0"), &ALL);
        assert_eq!(result, Vec::new());
    }

    #[test]
    fn explicit_zero_overrides_wildcard() {
        let result = negotiate_precompressed(Some("*;q=1, gzip;q=0"), &ALL);
        assert_eq!(result, vec![BROTLI, ZSTD,]);
    }

    #[test]
    fn server_order_restricts_candidates() {
        let result = negotiate_precompressed(Some("gzip, br, zstd"), &[GZIP]);
        assert_eq!(result, vec![GZIP]);
    }

    #[test]
    fn case_insensitive_coding_names() {
        let result = negotiate_precompressed(Some("GZIP"), &ALL);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].content_encoding, "gzip");
    }

    #[test]
    fn malformed_q_value_yields_empty() {
        assert_eq!(
            negotiate_precompressed(Some("gzip;q=notanumber"), &ALL),
            Vec::new()
        );
    }

    #[test]
    fn empty_header_yields_empty() {
        assert_eq!(negotiate_precompressed(Some(""), &ALL), Vec::new());
    }

    #[test]
    fn whitespace_tolerant() {
        let result = negotiate_precompressed(Some(" gzip ; q=0.5 , br ; q=1.0 "), &ALL);
        assert_eq!(result, vec![BROTLI, GZIP,]);
    }
}
