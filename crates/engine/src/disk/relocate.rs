//! Move torrent payload between data roots (Ctrl-O / leech_cache handoff).
//!
//! Live handoff must **publish dest before unpublishing source**. Per file:
//! hardlink when the same filesystem; on EXDEV (or hardlink unsupported) copy.
//! Source paths stay until the caller swaps `data_root` and unpublishes this
//! torrent's files (dedicated stage dirs may then wipe the tree).
//! Seed fill can open either name for that window.

use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::catalog::Catalog;
use crate::disk::spans::StorageLayout;
use crate::error::{Error, Result};

/// Publish every existing payload file from `layout.data_root` at `new_root`
/// (same relative paths). Source files are left in place.
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
        match publish_file(&src, &dst)? {
            PublishHow::Linked => stats.linked += 1,
            PublishHow::Copied => stats.copied += 1,
            PublishHow::SamePath => {}
        }
    }
    Ok(stats)
}

#[derive(Debug, Clone, Default)]
pub struct TransferStats {
    /// Same-FS hardlink (both paths exist).
    pub linked: u32,
    pub copied: u32,
    pub missing: u32,
}

#[derive(Debug, Clone, Copy)]
enum PublishHow {
    Linked,
    Copied,
    /// `src` and `dst` are the same directory entry — do not unpublish.
    SamePath,
}

/// Relocate torrent data from the current `meta_path.data_root` to `new_root`.
///
/// Offline / catalog-only path (torrent not hot). Hot path uses
/// [`crate::session::SessionRuntime::relocate_data_root`].
///
/// - Creates `new_root` if needed
/// - Publishes each existing file (hardlink / copy), then unpublishes sources
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
    unpublish_payload_files(&layout, &old_root, &new_root, false)?;
    tracing::info!(
        torrent_id,
        old = %old_root.display(),
        new = %new_root.display(),
        linked = stats.linked,
        copied = stats.copied,
        missing = stats.missing,
        "relocated torrent data"
    );
    Ok(())
}

/// Unlink this torrent's files under `old_root`.
///
/// Skips a name when it is the same directory entry as `new_root` (symlink
/// / same-root relocate). Prunes empty parents up to `old_root`. The root
/// stays if anything else remains. `wipe_root` is for a dedicated stage
/// (`{leech_cache}/{infohash}`) only.
pub fn unpublish_payload_files(
    layout: &StorageLayout,
    old_root: &Path,
    new_root: &Path,
    wipe_root: bool,
) -> Result<()> {
    for f in &layout.files {
        let src = old_root.join(&f.path);
        let dst = new_root.join(&f.path);
        if same_path(&src, &dst) {
            continue;
        }
        match fs::remove_file(&src) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(
                path = %src.display(),
                error = %e,
                "unpublish source failed"
            ),
        }
        remove_empty_parents(&src, old_root);
    }
    if wipe_root && !same_path(old_root, new_root) {
        if old_root.exists() {
            fs::remove_dir_all(old_root)
                .map_err(|e| Error::Path(old_root.to_path_buf(), e.to_string()))?;
        }
    } else if old_root.exists() {
        let _ = fs::remove_dir(old_root);
    }
    Ok(())
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

fn same_path(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

fn same_inode(a: &Path, b: &Path) -> bool {
    match (fs::metadata(a), fs::metadata(b)) {
        (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
        _ => false,
    }
}

fn publish_file(src: &Path, dst: &Path) -> Result<PublishHow> {
    if same_path(src, dst) {
        return Ok(PublishHow::SamePath);
    }
    if dst.exists() && !same_inode(src, dst) {
        fs::remove_file(dst).map_err(|e| Error::Path(dst.to_path_buf(), e.to_string()))?;
    }
    if dst.exists() && same_inode(src, dst) {
        return Ok(PublishHow::Linked);
    }
    match fs::hard_link(src, dst) {
        Ok(()) => Ok(PublishHow::Linked),
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            if same_path(src, dst) {
                return Ok(PublishHow::SamePath);
            }
            if same_inode(src, dst) {
                return Ok(PublishHow::Linked);
            }
            fs::remove_file(dst).map_err(|e| Error::Path(dst.to_path_buf(), e.to_string()))?;
            fs::copy(src, dst).map_err(|e| Error::Path(dst.to_path_buf(), e.to_string()))?;
            Ok(PublishHow::Copied)
        }
        Err(e) => {
            tracing::debug!(
                src = %src.display(),
                dst = %dst.display(),
                error = %e,
                "hardlink failed; copy (keep source)"
            );
            fs::copy(src, dst).map_err(|e| Error::Path(dst.to_path_buf(), e.to_string()))?;
            Ok(PublishHow::Copied)
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
    fn relocate_shared_root_keeps_siblings() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("lib");
        let dest = tmp.path().join("dest");
        fs::create_dir_all(&lib).unwrap();
        let payload = b"hello relocate";
        let tor = make_torrent(payload);
        let m = Metainfo::parse_bytes(&tor).unwrap();
        fs::File::create(lib.join("data"))
            .unwrap()
            .write_all(payload)
            .unwrap();
        fs::write(lib.join("other-torrent.bin"), b"keep me").unwrap();

        let db = tmp.path().join("c.sqlite");
        let mut cat = Catalog::open(&db).unwrap();
        let mut ins = TorrentInsert::from_metainfo(m, lib.display().to_string());
        ins.source_torrent = Some("x".into());
        let id = cat.insert_torrent(&ins).unwrap().id();

        relocate_torrent_data(&mut cat, id, &dest).unwrap();
        assert!(!lib.join("data").exists());
        assert!(dest.join("data").is_file());
        assert_eq!(fs::read(lib.join("other-torrent.bin")).unwrap(), b"keep me");
        assert!(lib.is_dir());
    }

    #[test]
    fn unpublish_wipe_root_removes_stage_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let stage = tmp.path().join("cache").join("abc123");
        fs::create_dir_all(stage.join("Show")).unwrap();
        fs::write(stage.join("Show").join("ep.mkv"), b"vid").unwrap();
        fs::write(stage.join("junk"), b"x").unwrap();
        let layout = StorageLayout {
            data_root: stage.clone(),
            piece_length: 16384,
            piece_count: 1,
            total_size: 3,
            files: vec![crate::disk::spans::FileLayout {
                path: PathBuf::from("Show/ep.mkv"),
                size: 3,
                offset: 0,
                priority: 1,
            }],
        };
        let dest = tmp.path().join("dest");
        unpublish_payload_files(&layout, &stage, &dest, true).unwrap();
        assert!(!stage.exists());
    }

    #[test]
    fn transfer_payload_files_keeps_source() {
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
        assert_eq!(st.linked + st.copied, 1);
        assert!(
            old.join("f.bin").is_file(),
            "source stays until caller deletes"
        );
        assert_eq!(fs::read(new.join("f.bin")).unwrap(), b"xyz");
    }

    fn one_file_layout(root: &Path, name: &str, size: u64) -> StorageLayout {
        StorageLayout {
            data_root: root.to_path_buf(),
            piece_length: 16384,
            piece_count: 1,
            total_size: size,
            files: vec![crate::disk::spans::FileLayout {
                path: PathBuf::from(name),
                size,
                offset: 0,
                priority: 1,
            }],
        }
    }

    #[test]
    fn publish_replaces_same_size_different_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let old = tmp.path().join("a");
        let new = tmp.path().join("b");
        fs::create_dir_all(&old).unwrap();
        fs::create_dir_all(&new).unwrap();
        fs::write(old.join("f.bin"), b"aaa").unwrap();
        fs::write(new.join("f.bin"), b"bbb").unwrap();
        let layout = one_file_layout(&old, "f.bin", 3);
        transfer_payload_files(&layout, &new).unwrap();
        assert_eq!(fs::read(new.join("f.bin")).unwrap(), b"aaa");
        unpublish_payload_files(&layout, &old, &new, false).unwrap();
        assert!(!old.join("f.bin").exists());
        assert_eq!(fs::read(new.join("f.bin")).unwrap(), b"aaa");
    }

    #[test]
    fn unpublish_after_hardlink_leaves_dest() {
        let tmp = tempfile::tempdir().unwrap();
        let old = tmp.path().join("a");
        let new = tmp.path().join("b");
        fs::create_dir_all(&old).unwrap();
        fs::create_dir_all(&new).unwrap();
        fs::write(old.join("f.bin"), b"xyz").unwrap();
        fs::hard_link(old.join("f.bin"), new.join("f.bin")).unwrap();
        let layout = one_file_layout(&old, "f.bin", 3);
        unpublish_payload_files(&layout, &old, &new, false).unwrap();
        assert!(!old.join("f.bin").exists());
        assert_eq!(fs::read(new.join("f.bin")).unwrap(), b"xyz");
    }

    #[test]
    fn relocate_to_symlink_of_current_root_keeps_payload() {
        let tmp = tempfile::tempdir().unwrap();
        let old = tmp.path().join("old");
        fs::create_dir_all(&old).unwrap();
        let payload = b"hello relocate";
        let tor = make_torrent(payload);
        let m = Metainfo::parse_bytes(&tor).unwrap();
        fs::File::create(old.join("data"))
            .unwrap()
            .write_all(payload)
            .unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&old, &link).unwrap();

        let db = tmp.path().join("c.sqlite");
        let mut cat = Catalog::open(&db).unwrap();
        let mut ins = TorrentInsert::from_metainfo(m, old.display().to_string());
        ins.source_torrent = Some("x".into());
        let id = cat.insert_torrent(&ins).unwrap().id();

        relocate_torrent_data(&mut cat, id, &link).unwrap();
        assert_eq!(fs::read(old.join("data")).unwrap(), payload);
        assert!(old.join("data").is_file());
    }
}
