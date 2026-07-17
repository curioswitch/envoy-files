//! Content-Type resolution from a file extension.
pub fn content_type_for<'a>(
    path: &str,
    overrides: &'a [(String, String)],
    default_content_type: &'a str,
) -> &'a str {
    let Some(extension) = extract_extension(path) else {
        return default_content_type;
    };

    for (key, value) in overrides {
        let trimmed_key = key.strip_prefix('.').unwrap_or(key);
        if trimmed_key.eq_ignore_ascii_case(extension) {
            return value;
        }
    }

    let lower = extension.to_ascii_lowercase();
    new_mime_guess::from_ext(&lower)
        .first_raw()
        .unwrap_or(default_content_type)
}

/// Returns the file extension of the final path component, or `None` when
/// there isn't one. `Path::extension` gives us the semantics we want for
/// free: no `.` → `None`, and a leading-dot dotfile with no other `.` (e.g.
/// `.gitignore`) has no extension. The `filter` drops a trailing-dot name
/// like `weird.` whose extension is empty.
fn extract_extension(path: &str) -> Option<&str> {
    std::path::Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .filter(|ext| !ext.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEFAULT: &str = "application/octet-stream";

    #[test]
    fn html_content_type() {
        assert_eq!(content_type_for("index.html", &[], DEFAULT), "text/html");
    }

    #[test]
    fn js_and_mjs_content_type() {
        // Both JavaScript media types are valid; new_mime_guess happens to map
        // .js to text/javascript (WHATWG-recommended) and .mjs to
        // application/javascript. Either is fine for browsers.
        assert_eq!(content_type_for("app.js", &[], DEFAULT), "text/javascript");
        assert_eq!(
            content_type_for("app.mjs", &[], DEFAULT),
            "application/javascript"
        );
    }

    #[test]
    fn json_content_type() {
        assert_eq!(
            content_type_for("data.json", &[], DEFAULT),
            "application/json"
        );
    }

    #[test]
    fn svg_content_type() {
        assert_eq!(content_type_for("icon.svg", &[], DEFAULT), "image/svg+xml");
    }

    #[test]
    fn wasm_has_no_charset() {
        assert_eq!(
            content_type_for("module.wasm", &[], DEFAULT),
            "application/wasm"
        );
    }

    #[test]
    fn png_binary_has_no_charset() {
        assert_eq!(content_type_for("photo.png", &[], DEFAULT), "image/png");
    }

    #[test]
    fn extensionless_uses_default() {
        assert_eq!(content_type_for("README", &[], DEFAULT), DEFAULT);
    }

    #[test]
    fn unknown_extension_uses_default() {
        assert_eq!(content_type_for("file.zzz", &[], DEFAULT), DEFAULT);
    }

    #[test]
    fn case_insensitive_extension_match() {
        assert_eq!(content_type_for("IMAGE.PNG", &[], DEFAULT), "image/png");
        assert_eq!(content_type_for("INDEX.HTML", &[], DEFAULT), "text/html");
    }

    #[test]
    fn override_without_dot_prefix_matches() {
        let overrides = vec![("wasm".to_string(), "application/x-custom-wasm".to_string())];
        assert_eq!(
            content_type_for("module.wasm", &overrides, DEFAULT),
            "application/x-custom-wasm"
        );
    }

    #[test]
    fn override_with_dot_prefix_matches() {
        let overrides = vec![(".wasm".to_string(), "application/x-custom-wasm".to_string())];
        assert_eq!(
            content_type_for("module.wasm", &overrides, DEFAULT),
            "application/x-custom-wasm"
        );
    }

    #[test]
    fn override_takes_priority_over_builtin() {
        let overrides = vec![("html".to_string(), "text/plain".to_string())];
        assert_eq!(
            content_type_for("index.html", &overrides, DEFAULT),
            "text/plain"
        );
    }

    #[test]
    fn override_is_case_insensitive_on_key() {
        let overrides = vec![("HTML".to_string(), "text/plain".to_string())];
        assert_eq!(
            content_type_for("index.html", &overrides, DEFAULT),
            "text/plain"
        );
    }

    #[test]
    fn path_with_directories_uses_final_segment() {
        assert_eq!(
            content_type_for("a/b/c/index.html", &[], DEFAULT),
            "text/html"
        );
    }

    #[test]
    fn dotfile_with_no_other_dot_is_extensionless() {
        assert_eq!(content_type_for(".gitignore", &[], DEFAULT), DEFAULT);
    }

    #[test]
    fn multiple_dots_uses_last_extension() {
        assert_eq!(
            content_type_for("archive.tar.gz", &[], DEFAULT),
            "application/gzip"
        );
    }

    #[test]
    fn markdown_extension() {
        assert_eq!(content_type_for("README.md", &[], DEFAULT), "text/markdown");
    }

    #[test]
    fn yaml_and_yml() {
        assert_eq!(content_type_for("a.yaml", &[], DEFAULT), "text/yaml");
        assert_eq!(content_type_for("a.yml", &[], DEFAULT), "text/yaml");
    }

    #[test]
    fn map_is_json() {
        assert_eq!(
            content_type_for("app.js.map", &[], DEFAULT),
            "application/json"
        );
    }

    #[test]
    fn trailing_dot_has_empty_extension() {
        assert_eq!(content_type_for("weird.", &[], DEFAULT), DEFAULT);
    }
}
