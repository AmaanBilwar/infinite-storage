//! Platform-neutral mapping between WinFSP virtual paths and S3 object keys.
//!
//! The Windows-only WinFSP layer ([`crate::winfsp_fs`]) presents a configured
//! drive as a flat volume. These helpers translate back and forth between the
//! backslash-separated virtual paths WinFSP reports and the slash-separated
//! keys S3 expects. They live in their own module with no Windows
//! dependencies so the mapping stays unit-testable on any platform.
#![cfg_attr(not(windows), allow(dead_code))]

/// Convert a WinFSP virtual path to an S3 object key.
///
/// Accepts `\`, `/`, or mixed separators:
/// - `\photos\a.jpg`, `/photos/a.jpg` and `photos/a.jpg` all map to
///   `Some("photos/a.jpg")`
/// - the root (`\`, `/`, `""`) maps to `None`
/// - `.` segments are ignored; paths containing `..` map to `None` so a
///   handle can never escape the bucket namespace.
pub fn s3_key_from_win_path(path: &str) -> Option<String> {
    let parts: Vec<&str> = path
        .split(['/', '\\'])
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();
    if parts.is_empty() || parts.iter().any(|p| *p == "..") {
        return None;
    }
    Some(parts.join("/"))
}

/// Convert an S3 object key back to a WinFSP virtual path.
///
/// - `"photos/a.jpg"` maps to `"\photos\a.jpg"`
/// - the empty key maps to the root `"\"`.
pub fn win_path_from_s3_key(key: &str) -> String {
    let parts: Vec<&str> = key
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();
    if parts.is_empty() {
        return "\\".to_owned();
    }
    format!("\\{}", parts.join("\\"))
}

/// Returns true for the filesystem root (`\`, `/`, or empty/blank).
pub fn is_root_path(path: &str) -> bool {
    let stripped: String = path
        .split(['/', '\\'])
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();
    stripped.is_empty()
}

/// Base name of an S3 key (`"a/b/c.txt"` -> `"c.txt"`).
///
/// Trailing slashes are ignored (`"a/b/"` -> `"b"`); an empty key yields `""`.
pub fn file_name_from_key(key: &str) -> &str {
    key.split('/')
        .filter(|s| !s.is_empty())
        .next_back()
        .unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_paths_map_to_none() {
        for root in ["\\", "/", "", "\\\\", "//", ".", "\\.\\"] {
            assert_eq!(s3_key_from_win_path(root), None, "path: {root:?}");
            assert!(is_root_path(root), "path: {root:?}");
        }
    }

    #[test]
    fn nested_win_path_maps_to_key() {
        assert_eq!(
            s3_key_from_win_path("\\photos\\2024\\a.jpg"),
            Some("photos/2024/a.jpg".to_owned())
        );
        assert_eq!(
            s3_key_from_win_path("/photos/2024/a.jpg"),
            Some("photos/2024/a.jpg".to_owned())
        );
        assert_eq!(
            s3_key_from_win_path("photos\\mixed/a.jpg"),
            Some("photos/mixed/a.jpg".to_owned())
        );
    }

    #[test]
    fn dot_segments_are_ignored_but_dotdot_is_rejected() {
        assert_eq!(
            s3_key_from_win_path("\\photos\\.\\a.jpg"),
            Some("photos/a.jpg".to_owned())
        );
        assert_eq!(s3_key_from_win_path("\\photos\\..\\a.jpg"), None);
        assert_eq!(s3_key_from_win_path(".."), None);
    }

    #[test]
    fn key_to_win_path_roundtrip() {
        for key in ["a.jpg", "photos/a.jpg", "photos/2024/a.jpg"] {
            let win = win_path_from_s3_key(key);
            assert_eq!(s3_key_from_win_path(&win).as_deref(), Some(key));
        }
        assert_eq!(win_path_from_s3_key(""), "\\");
        assert_eq!(win_path_from_s3_key("photos/a.jpg"), "\\photos\\a.jpg");
    }

    #[test]
    fn non_root_paths_are_not_root() {
        for path in ["\\a.jpg", "photos", "\\photos\\a.jpg", "C"] {
            assert!(!is_root_path(path), "path: {path:?}");
        }
    }

    #[test]
    fn file_name_extraction() {
        assert_eq!(file_name_from_key("a.jpg"), "a.jpg");
        assert_eq!(file_name_from_key("photos/a.jpg"), "a.jpg");
        assert_eq!(file_name_from_key("a/b/"), "b");
        assert_eq!(file_name_from_key(""), "");
    }
}
