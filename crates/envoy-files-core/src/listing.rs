//! Deterministic HTML directory listing rendering.

use html_escape::encode_text;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};

use crate::validators::format_http_date;

/// Characters to percent-encode in an `href` path segment. We take the default
/// non-alphanumeric and allow a few characters that don't need escaping and appear
/// commonly in filenames.
const PATH_SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// One entry in a directory listing.
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime_unix: i64,
}

/// Renders a directory listing as a small, dependency-free HTML page.
/// `url_path` is the request path the listing is served for (the page
/// heading). `at_root` must be true when this directory is the root of the
/// served tree; the `../` parent link is emitted only when it is false.
pub fn render_listing(url_path: &str, at_root: bool, entries: &[DirEntry]) -> String {
    let mut sorted: Vec<&DirEntry> = entries.iter().collect();
    sorted.sort_by(|a, b| {
        // Directories first, then case-insensitive alphabetical, with a
        // stable tie-break on the exact name so identically-cased-but-
        // distinct entries (e.g. differing only by non-ASCII case folding)
        // still sort deterministically.
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });

    let mut html = String::new();
    html.push_str("<!DOCTYPE html>\n<html>\n<head>\n");
    html.push_str("<meta charset=\"utf-8\">\n");
    html.push_str(&format!(
        "<title>Index of {}</title>\n",
        encode_text(url_path)
    ));
    html.push_str("</head>\n<body>\n");
    html.push_str(&format!("<h1>Index of {}</h1>\n", encode_text(url_path)));
    html.push_str("<table>\n");
    html.push_str("<tr><th>Name</th><th>Size</th><th>Last modified</th></tr>\n");

    if !at_root {
        html.push_str("<tr><td><a href=\"../\">../</a></td><td></td><td></td></tr>\n");
    }

    for entry in sorted {
        let href = percent_encode_path_segment(&entry.name);
        let display_name = encode_text(&entry.name).into_owned();
        let (href, display_name) = if entry.is_dir {
            (format!("{href}/"), format!("{display_name}/"))
        } else {
            (href, display_name)
        };
        let size_cell = if entry.is_dir {
            String::new()
        } else {
            humanize_size(entry.size)
        };
        let modified_cell = format_http_date(entry.mtime_unix);

        html.push_str(&format!(
            "<tr><td><a href=\"{href}\">{display_name}</a></td><td>{size_cell}</td><td>{modified_cell}</td></tr>\n"
        ));
    }

    html.push_str("</table>\n</body>\n</html>\n");
    html
}

/// Percent-encodes a filename for use as an `href` path segment (see
/// [`PATH_SEGMENT`]).
fn percent_encode_path_segment(name: &str) -> String {
    utf8_percent_encode(name, PATH_SEGMENT).to_string()
}

/// Formats a byte count using binary (1024-based) units with one decimal
/// place, e.g. `1.5 KiB`. Byte counts under 1 KiB are shown as a bare
/// integer, e.g. `512 B`.
fn humanize_size(size: u64) -> String {
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];

    if size < 1024 {
        return format!("{size} B");
    }

    let mut value = size as f64 / 1024.0;
    let mut unit_index = 0;
    while value >= 1024.0 && unit_index < UNITS.len() - 1 {
        value /= 1024.0;
        unit_index += 1;
    }

    format!("{:.1} {}", value, UNITS[unit_index])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, is_dir: bool, size: u64, mtime_unix: i64) -> DirEntry {
        DirEntry {
            name: name.to_string(),
            is_dir,
            size,
            mtime_unix,
        }
    }

    #[test]
    fn root_path_has_no_parent_link() {
        let html = render_listing("/", true, &[]);
        assert!(!html.contains("../"));
    }

    #[test]
    fn non_root_path_has_parent_link() {
        let html = render_listing("/sub/", false, &[entry("a.txt", false, 10, 0)]);
        assert!(html.contains("<a href=\"../\">../</a>"));
    }

    #[test]
    fn root_at_non_slash_url_still_has_no_parent_link() {
        // A served root mounted at a non-"/" URL must not offer a link above
        // itself, even though the heading path is not "/".
        let html = render_listing("/static/", true, &[entry("a.txt", false, 10, 0)]);
        assert!(!html.contains("../"));
    }

    #[test]
    fn directories_sort_before_files() {
        let entries = vec![
            entry("zeta.txt", false, 1, 0),
            entry("alpha_dir", true, 0, 0),
        ];
        let html = render_listing("/", true, &entries);
        let dir_pos = html.find("alpha_dir").unwrap();
        let file_pos = html.find("zeta.txt").unwrap();
        assert!(dir_pos < file_pos);
    }

    #[test]
    fn case_insensitive_alphabetical_order() {
        let entries = vec![
            entry("Banana.txt", false, 1, 0),
            entry("apple.txt", false, 1, 0),
            entry("Cherry.txt", false, 1, 0),
        ];
        let html = render_listing("/", true, &entries);
        let apple = html.find("apple.txt").unwrap();
        let banana = html.find("Banana.txt").unwrap();
        let cherry = html.find("Cherry.txt").unwrap();
        assert!(apple < banana);
        assert!(banana < cherry);
    }

    #[test]
    fn directories_get_trailing_slash_in_name_and_href() {
        let html = render_listing("/", true, &[entry("docs", true, 0, 0)]);
        assert!(html.contains("href=\"docs/\""));
        assert!(html.contains(">docs/<"));
    }

    #[test]
    fn xss_script_tag_name_is_escaped() {
        let html = render_listing(
            "/",
            true,
            &[entry("<script>alert(1)</script>", false, 1, 0)],
        );
        assert!(!html.contains("<script>alert(1)</script>"));
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    }

    #[test]
    fn quote_in_name_is_percent_encoded_in_href() {
        let html = render_listing("/", true, &[entry("a\"b", false, 1, 0)]);
        // The quote is percent-encoded in the href (so it can't break out of
        // the attribute); in the text cell it's harmless and left as-is.
        assert!(html.contains("href=\"a%22b\""));
        assert!(!html.contains("href=\"a\"b\""));
        assert!(html.contains(">a\"b<"));
    }

    #[test]
    fn percent_in_name_is_percent_encoded() {
        let html = render_listing("/", true, &[entry("100%.txt", false, 1, 0)]);
        assert!(html.contains("href=\"100%25.txt\""));
        assert!(html.contains(">100%.txt<"));
    }

    #[test]
    fn size_humanization() {
        assert_eq!(humanize_size(0), "0 B");
        assert_eq!(humanize_size(1023), "1023 B");
        assert_eq!(humanize_size(1024), "1.0 KiB");
        assert_eq!(humanize_size(1536), "1.5 KiB");
        assert_eq!(humanize_size(1024 * 1024), "1.0 MiB");
        assert_eq!(humanize_size(1024 * 1024 * 1024), "1.0 GiB");
    }

    #[test]
    fn mtime_uses_http_date_format() {
        let html = render_listing("/", true, &[entry("a.txt", false, 1, 0)]);
        assert!(html.contains("Thu, 01 Jan 1970 00:00:00 GMT"));
    }

    #[test]
    fn empty_directory_renders_without_entries() {
        let html = render_listing("/", true, &[]);
        assert!(html.contains("<table>"));
        assert!(!html.contains("<tr><td><a href=\""));
    }
}
