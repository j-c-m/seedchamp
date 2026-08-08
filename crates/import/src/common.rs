//! Shared import options, report, and path helpers.

use std::fs;
use std::path::Path;
use std::time::SystemTime;

/// Import options (rtorrent and transmission).
#[derive(Debug, Clone)]
pub struct ImportOptions {
    pub dry_run: bool,
    pub start_after: bool,
    /// Default data root when session has no download directory.
    pub default_data_root: String,
}

impl Default for ImportOptions {
    fn default() -> Self {
        Self {
            dry_run: false,
            start_after: false,
            default_data_root: ".".into(),
        }
    }
}

#[derive(Debug, Default)]
pub struct ImportReport {
    pub scanned: u32,
    pub imported: u32,
    /// Already in catalog; metadata may have been refreshed (timestamps/stats).
    pub skipped: u32,
    /// Existing rows whose created_at / finished_at / stats were updated.
    pub updated: u32,
    /// Sum of lifetime upload bytes applied from session.
    pub uploaded_bytes: u64,
    /// Sum of lifetime download bytes applied from session.
    pub downloaded_bytes: u64,
    /// How many torrents had non-zero up or down totals in the session files.
    pub with_transfer_stats: u32,
    pub errors: Vec<String>,
}

pub(crate) fn file_mtime_unix(path: &Path) -> Option<i64> {
    let meta = fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    Some(
        modified
            .duration_since(SystemTime::UNIX_EPOCH)
            .ok()?
            .as_secs() as i64,
    )
}

/// If `data_root` ends with torrent `name` (multi-file root), return parent.
pub(crate) fn strip_trailing_torrent_name(data_root: &str, name: &str) -> String {
    let p = Path::new(data_root);
    if p.file_name().and_then(|s| s.to_str()) == Some(name) {
        if let Some(parent) = p.parent() {
            if !parent.as_os_str().is_empty() {
                return parent.display().to_string();
            }
        }
    }
    data_root.to_string()
}

/// `40 hex chars` + `.torrent` (uppercase or lowercase).
pub(crate) fn is_infohash_torrent_name(name: &str) -> bool {
    if name.len() != 48 || !name.ends_with(".torrent") {
        return false;
    }
    let hex = &name[..40];
    hex.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_name_filter() {
        assert!(is_infohash_torrent_name(
            "0123456789ABCDEF0123456789ABCDEF01234567.torrent"
        ));
        assert!(!is_infohash_torrent_name("foo.torrent"));
        assert!(!is_infohash_torrent_name(
            "0123456789ABCDEF0123456789ABCDEF01234567.torrent.rtorrent"
        ));
    }

    #[test]
    fn strip_multi_file_directory_name() {
        assert_eq!(
            strip_trailing_torrent_name("/dl/MyTorrent", "MyTorrent"),
            "/dl"
        );
        assert_eq!(
            strip_trailing_torrent_name("/dl/other", "MyTorrent"),
            "/dl/other"
        );
    }
}
