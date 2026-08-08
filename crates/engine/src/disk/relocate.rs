//! Move torrent payload between data roots (Ctrl-O / leech_cache handoff).
//!
//! Per file: rename when same filesystem; on EXDEV copy then delete.
//! Same-FS rename keeps the inode so open peer FDs stay valid.

use std::fs;
use std::path::{Path, PathBuf};

use crate::catalog::Catalog;
use crate::disk::spans::StorageLayout;
use crate::error::{Error, Result};

/// Move every existing payload file from `layout.data_root` to `new_root`
/// (same relative paths). Rename when same filesystem; on EXDEV copy then delete.
///
/// Does **not** update the catalog. Missing sources are skipped (counted).
/// Creates `new_root` as needed.
pub fn transfer_payload_files(layout: &StorageLayout, new_root: &Path) -> Result<TransferStats> {
    let old_root = &layout.data_root;
    let new_root = new_root.to_path_buf();

    if old_root == &new_root {
        return Ok(TransferStats::default());
    }

    fs::create_dir_all(&new_root).map_err(|e| Error::Path(new_root.clone(), e.to_string()))?;

    let mut stats = TransferStats::default();
    for f in &layout.files {
        let src = old_root.join(&f.path);
        let dst = new_root.join(&f.path);
        if !src.exists() {
            stats.missing += 1;
            continue;
        }
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| Error::Path(parent.to_path_buf(), e.to_string()))?;
        }
        let how = move_file(&src, &dst)?;
        match how {
            MoveHow::Rename => stats.renamed += 1,
            MoveHow::CopyDelete => stats.copied += 1,
        }
        // Best-effort: prune empty parents under old root (not the root itself).
        remove_empty_parents(&src, old_root);
    }
    Ok(stats)
}

#[derive(Debug, Clone, Default)]
pub struct TransferStats {
    pub renamed: u32,
    pub copied: u32,
    pub missing: u32,
}

#[derive(Debug, Clone, Copy)]
enum MoveHow {
    Rename,
    CopyDelete,
}

/// Relocate torrent data from the current `meta_path.data_root` to `new_root`.
///
/// Offline / catalog-only path (torrent not hot). Hot path uses
/// [`crate::session::SessionRuntime::relocate_data_root`].
///
/// - Creates `new_root` if needed
/// - Renames each existing file (copy+delete cross-device)
/// - Updates catalog `meta_path.data_root`
pub fn relocate_torrent_data(
    catalog: &mut Catalog,
    torrent_id: i64,
    new_root: &Path,
) -> Result<()> {
    let layout = catalog.load_storage_layout(torrent_id)?;
    let old_root = layout.data_root.clone();
    let new_root = new_root.to_path_buf();

    if old_root == new_root {
        return Ok(());
    }

    let stats = transfer_payload_files(&layout, &new_root)?;
    catalog.set_data_root(torrent_id, &new_root)?;
    tracing::info!(
        torrent_id,
        old = %old_root.display(),
        new = %new_root.display(),
        renamed = stats.renamed,
        copied = stats.copied,
        missing = stats.missing,
        "relocated torrent data"
    );
    Ok(())
}

fn move_file(src: &Path, dst: &Path) -> Result<MoveHow> {
    if src == dst {
        return Ok(MoveHow::Rename);
    }
    match fs::rename(src, dst) {
        Ok(()) => Ok(MoveHow::Rename),
        Err(e) => {
            // Cross-device: copy then remove.
            tracing::debug!(
                src = %src.display(),
                dst = %dst.display(),
                error = %e,
                "rename failed; copy+delete"
            );
            fs::copy(src, dst).map_err(|e| Error::Path(dst.to_path_buf(), e.to_string()))?;
            fs::remove_file(src).map_err(|e| Error::Path(src.to_path_buf(), e.to_string()))?;
            Ok(MoveHow::CopyDelete)
        }
    }
}

fn remove_empty_parents(file: &Path, stop_at: &Path) {
    let mut cur = file.parent().map(Path::to_path_buf);
    while let Some(dir) = cur {
        if dir == *stop_at {
            break;
        }
        match fs::remove_dir(&dir) {
            Ok(()) => cur = dir.parent().map(Path::to_path_buf),
            Err(_) => break,
        }
    }
}

/// Resolve a user-entered path (expand `~/`).
pub fn expand_user_path(s: &str) -> PathBuf {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
            return PathBuf::from(home).join(rest);
        }
    }
    if s == "~" {
        if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
            return PathBuf::from(home);
        }
    }
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::TorrentInsert;
    use crate::metainfo::Metainfo;
    use sha1::{Digest, Sha1};
    use std::io::Write;

    fn make_torrent(payload: &[u8]) -> Vec<u8> {
        let mut h = Sha1::new();
        h.update(payload);
        let pieces = h.finalize().to_vec();
        let mut info = Vec::new();
        info.extend_from_slice(format!("d6:lengthi{}e", payload.len()).as_bytes());
        info.extend_from_slice(b"4:name4:data12:piece lengthi16384e6:pieces20:");
        info.extend_from_slice(&pieces);
        info.extend_from_slice(b"e");
        let mut root = Vec::new();
        root.extend_from_slice(b"d8:announce8:http://x4:info");
        root.extend_from_slice(&info);
        root.extend_from_slice(b"e");
        root
    }

    #[test]
    fn relocate_moves_file_and_updates_catalog() {
        let tmp = tempfile::tempdir().unwrap();
        let old = tmp.path().join("old");
        let new = tmp.path().join("new");
        fs::create_dir_all(&old).unwrap();
        let payload = b"hello relocate";
        let tor = make_torrent(payload);
        let m = Metainfo::parse_bytes(&tor).unwrap();
        fs::File::create(old.join("data"))
            .unwrap()
            .write_all(payload)
            .unwrap();

        let db = tmp.path().join("c.sqlite");
        let mut cat = Catalog::open(&db).unwrap();
        let mut ins = TorrentInsert::from_metainfo(m, old.display().to_string());
        ins.source_torrent = Some("x".into());
        let id = cat.insert_torrent(&ins).unwrap().id();

        relocate_torrent_data(&mut cat, id, &new).unwrap();
        assert!(!old.join("data").exists());
        assert!(new.join("data").is_file());
        let layout = cat.load_storage_layout(id).unwrap();
        assert_eq!(layout.data_root, new);
    }

    #[test]
    fn transfer_payload_files_rename() {
        let tmp = tempfile::tempdir().unwrap();
        let old = tmp.path().join("a");
        let new = tmp.path().join("b");
        fs::create_dir_all(&old).unwrap();
        fs::write(old.join("f.bin"), b"xyz").unwrap();
        let layout = StorageLayout {
            data_root: old.clone(),
            piece_length: 16384,
            piece_count: 1,
            total_size: 3,
            files: vec![crate::disk::spans::FileLayout {
                path: PathBuf::from("f.bin"),
                size: 3,
                offset: 0,
                priority: 1,
            }],
        };
        let st = transfer_payload_files(&layout, &new).unwrap();
        assert_eq!(st.renamed, 1);
        assert!(!old.join("f.bin").exists());
        assert_eq!(fs::read(new.join("f.bin")).unwrap(), b"xyz");
    }
}
