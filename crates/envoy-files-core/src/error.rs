//! Mapping from filesystem errors to HTTP status codes.

use std::io::ErrorKind;

use http::StatusCode;

/// `errno` value for `ELOOP` ("too many levels of symbolic links"), which
/// differs across platforms and has no stable `std::io::ErrorKind` variant
/// as of this crate's MSRV. Used only as a fallback when `kind` itself
/// isn't informative enough.
#[cfg(target_os = "linux")]
const ELOOP: i32 = 40;
#[cfg(target_os = "macos")]
const ELOOP: i32 = 62;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
const ELOOP: i32 = -1;

/// Maps an I/O error observed while resolving/serving a file to the HTTP
/// status code that should be returned for it.
///
/// Note: `IsADirectory` is included for completeness, but callers normally
/// intercept directory requests earlier (to redirect or list them) and
/// won't see this from a plain read attempt.
pub fn status_for_io_error(kind: ErrorKind, raw_os_error: Option<i32>) -> StatusCode {
    match kind {
        ErrorKind::NotFound => StatusCode::NOT_FOUND,
        ErrorKind::PermissionDenied => StatusCode::FORBIDDEN,
        ErrorKind::NotADirectory => StatusCode::NOT_FOUND,
        ErrorKind::IsADirectory => StatusCode::FORBIDDEN,
        _ => {
            if let Some(errno) = raw_os_error
                && errno == ELOOP
            {
                return StatusCode::FORBIDDEN;
            }
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_found_maps_to_404() {
        assert_eq!(
            status_for_io_error(ErrorKind::NotFound, None),
            StatusCode::NOT_FOUND
        );
    }

    #[test]
    fn permission_denied_maps_to_403() {
        assert_eq!(
            status_for_io_error(ErrorKind::PermissionDenied, None),
            StatusCode::FORBIDDEN
        );
    }

    #[test]
    fn not_a_directory_maps_to_404() {
        assert_eq!(
            status_for_io_error(ErrorKind::NotADirectory, None),
            StatusCode::NOT_FOUND
        );
    }

    #[test]
    fn is_a_directory_maps_to_403() {
        assert_eq!(
            status_for_io_error(ErrorKind::IsADirectory, None),
            StatusCode::FORBIDDEN
        );
    }

    #[test]
    fn eloop_errno_maps_to_403() {
        assert_eq!(
            status_for_io_error(ErrorKind::Other, Some(ELOOP)),
            StatusCode::FORBIDDEN
        );
    }

    #[test]
    fn unknown_error_maps_to_500() {
        assert_eq!(
            status_for_io_error(ErrorKind::Other, None),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(
            status_for_io_error(ErrorKind::Other, Some(9999)),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[test]
    fn unrelated_kind_with_eloop_errno_still_maps_via_kind_first() {
        // NotFound takes priority over any raw_os_error interpretation.
        assert_eq!(
            status_for_io_error(ErrorKind::NotFound, Some(ELOOP)),
            StatusCode::NOT_FOUND
        );
    }
}
