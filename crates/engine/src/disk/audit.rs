//! Startup storage integrity: stat payload files for complete torrents.
//!
//! No create / set_len — only diagnose. Wrong size (short **or** long), missing,
//! or non-file paths are failures.

use std::fs;
use std::path::PathBuf;

use super::spans::StorageLayout;

/// Why a complete torrent's on-disk file failed the integrity check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageProblemKind {
    /// Path does not exist.
    Missing,
    /// Exists but is not a regular file.
    NotFile,
    /// Regular file whose length is not exactly the torrent file size.
    WrongSize,
    /// `metadata` / stat failed for another reason.
    Io,
}

/// First failing file for a layout (relative path as stored in the catalog).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageFileProblem {
    /// Path relative to `data_root` (catalog form).
    pub path: PathBuf,
    /// Absolute path used for stat.
    pub absolute: PathBuf,
    pub expected: u64,
    /// `None` when missing / not a file / IO error without a usable size.
    pub actual: Option<u64>,
    pub kind: StorageProblemKind,
}

impl StorageFileProblem {
    /// Short message for `torrent.error_msg` / logs.
    pub fn error_msg(&self) -> String {
        let rel = self.path.display();
        match self.kind {
            StorageProblemKind::Missing => {
                format!("storage missing: {rel}")
            }
            StorageProblemKind::NotFile => {
                format!("storage not a file: {rel}")
            }
            StorageProblemKind::WrongSize => {
                let actual = self.actual.unwrap_or(0);
                format!(
                    "storage size mismatch: {rel} expected={} actual={actual}",
                    self.expected
                )
            }
            StorageProblemKind::Io => {
                format!("storage unreadable: {rel}")
            }
        }
    }
}

/// Stat every layout file; require a regular file with **exact** size.
///
/// Checks **all** files (including priority 0): full-torrent `complete=1` needs
/// every piece's bytes on disk. Does not create or truncate anything.
///
/// Returns `None` if every file is present with `len == expected`.
pub fn check_complete_layout(layout: &StorageLayout) -> Option<StorageFileProblem> {
    for f in &layout.files {
        let absolute = layout.absolute(&f.path);
        match fs::metadata(&absolute) {
            Ok(meta) => {
                if !meta.is_file() {
                    return Some(StorageFileProblem {
                        path: f.path.clone(),
                        absolute,
                        expected: f.size,
                        actual: None,
                        kind: StorageProblemKind::NotFile,
                    });
                }
                let len = meta.len();
                if len != f.size {
                    return Some(StorageFileProblem {
                        path: f.path.clone(),
                        absolute,
                        expected: f.size,
                        actual: Some(len),
                        kind: StorageProblemKind::WrongSize,
                    });
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Some(StorageFileProblem {
                    path: f.path.clone(),
                    absolute,
                    expected: f.size,
                    actual: None,
                    kind: StorageProblemKind::Missing,
                });
            }
            Err(_) => {
                return Some(StorageFileProblem {
                    path: f.path.clone(),
                    absolute,
                    expected: f.size,
                    actual: None,
                    kind: StorageProblemKind::Io,
                });
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk::spans::FileLayout;
    use std::io::Write;
    use std::path::PathBuf;

    fn layout_one(dir: &std::path::Path, name: &str, size: u64) -> StorageLayout {
        StorageLayout {
            data_root: dir.to_path_buf(),
            piece_length: 16384,
            piece_count: 1,
            total_size: size,
            files: vec![FileLayout {
                path: PathBuf::from(name),
                size,
                offset: 0,
                priority: 1,
            }],
        }
    }

    #[test]
    fn ok_exact_size() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.bin");
        std::fs::write(&p, vec![0u8; 32]).unwrap();
        let lay = layout_one(dir.path(), "a.bin", 32);
        assert!(check_complete_layout(&lay).is_none());
    }

    #[test]
    fn ok_zero_length_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("empty");
        std::fs::File::create(&p).unwrap();
        let lay = layout_one(dir.path(), "empty", 0);
        assert!(check_complete_layout(&lay).is_none());
    }

    #[test]
    fn missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let lay = layout_one(dir.path(), "gone.bin", 10);
        let p = check_complete_layout(&lay).expect("missing");
        assert_eq!(p.kind, StorageProblemKind::Missing);
        assert!(p.error_msg().contains("missing"));
    }

    #[test]
    fn short_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.bin"), vec![1u8; 8]).unwrap();
        let lay = layout_one(dir.path(), "a.bin", 32);
        let p = check_complete_layout(&lay).expect("short");
        assert_eq!(p.kind, StorageProblemKind::WrongSize);
        assert_eq!(p.actual, Some(8));
        assert_eq!(p.expected, 32);
    }

    #[test]
    fn long_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.bin"), vec![1u8; 64]).unwrap();
        let lay = layout_one(dir.path(), "a.bin", 32);
        let p = check_complete_layout(&lay).expect("long");
        assert_eq!(p.kind, StorageProblemKind::WrongSize);
        assert_eq!(p.actual, Some(64));
    }

    #[test]
    fn not_a_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("a.bin")).unwrap();
        let lay = layout_one(dir.path(), "a.bin", 1);
        let p = check_complete_layout(&lay).expect("dir");
        assert_eq!(p.kind, StorageProblemKind::NotFile);
    }

    #[test]
    fn multi_file_second_bad() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("ok.bin"), vec![0u8; 4]).unwrap();
        // second missing
        let lay = StorageLayout {
            data_root: dir.path().to_path_buf(),
            piece_length: 16384,
            piece_count: 1,
            total_size: 8,
            files: vec![
                FileLayout {
                    path: PathBuf::from("ok.bin"),
                    size: 4,
                    offset: 0,
                    priority: 1,
                },
                FileLayout {
                    path: PathBuf::from("bad.bin"),
                    size: 4,
                    offset: 4,
                    priority: 0, // still checked for complete=1
                },
            ],
        };
        let p = check_complete_layout(&lay).expect("second");
        assert_eq!(p.path, PathBuf::from("bad.bin"));
        assert_eq!(p.kind, StorageProblemKind::Missing);
    }

    #[test]
    fn does_not_create_missing() {
        let dir = tempfile::tempdir().unwrap();
        let lay = layout_one(dir.path(), "nope.bin", 100);
        let _ = check_complete_layout(&lay);
        assert!(!dir.path().join("nope.bin").exists());
        // ensure we didn't leave anything behind
        let _ = std::io::stdout().flush();
    }
}
