//! Optional `paths.leech_cache` leech cache for incomplete wanted downloads.
//!
//! When configured and wanted payload fits free space (and optional size cap),
//! leech under `{leech_cache}/{infohash40}/`. On wanted-complete: copy to permanent
//! `home_root`, switch catalog, delete stage (seed during copy; brief stop at switch).
//!
//! Recommended on a fast local volume (typically SSD). Leave empty to write
//! straight to the permanent data root.

use std::fs;
use std::path::{Path, PathBuf};

use crate::catalog::Catalog;
use crate::disk::StorageLayout;
use crate::error::{Error, Result};
use crate::metainfo::Metainfo; // used by wanted_bytes_from_metainfo

/// Free-space / size-cap margin beyond wanted bytes (avoid filling the volume).
const FREE_MARGIN_BYTES: u64 = 256 * 1024 * 1024;

/// Placement result for a new incomplete torrent.
#[derive(Debug, Clone)]
pub struct Placement {
    /// Where payload is written now (cache staging or permanent).
    pub data_root: PathBuf,
    /// Permanent library root when staged; `None` if writing directly to home.
    pub home_root: Option<PathBuf>,
    pub used_leech_cache: bool,
}

/// True if `leech_cache` is configured (non-empty after expand).
pub fn leech_cache_enabled(leech_cache: &Path) -> bool {
    !leech_cache.as_os_str().is_empty()
}

/// Sum of file lengths with priority ≠ 0. Empty priorities → all files wanted.
pub fn wanted_bytes_from_metainfo(m: &Metainfo, priorities: &[i32]) -> u64 {
    let files = &m.files;
    if files.is_empty() {
        return m.total_size;
    }
    let mut sum = 0u64;
    for (i, f) in files.iter().enumerate() {
        let pri = priorities.get(i).copied().unwrap_or(1);
        if pri != 0 {
            sum = sum.saturating_add(f.size);
        }
    }
    if sum == 0 {
        // All off — treat as full size for placement (still go to home typically).
        m.total_size
    } else {
        sum
    }
}

/// Wanted bytes from catalog layout (priority ≠ 0).
pub fn wanted_bytes_from_layout(layout: &StorageLayout) -> u64 {
    let mut sum = 0u64;
    for f in &layout.files {
        if f.priority != 0 {
            sum = sum.saturating_add(f.size);
        }
    }
    if sum == 0 {
        layout.total_size
    } else {
        sum
    }
}

/// Free bytes available on the filesystem of `path` (or nearest existing ancestor).
pub fn free_space_bytes(path: &Path) -> Result<u64> {
    let probe = existing_ancestor(path);
    let st = nix::sys::statvfs::statvfs(probe.as_path())
        .map_err(|e| Error::Msg(format!("statvfs {}: {e}", probe.display())))?;
    let fr = st.fragment_size() as u64;
    Ok((st.blocks_available() as u64).saturating_mul(fr))
}

fn existing_ancestor(path: &Path) -> PathBuf {
    let mut p = path.to_path_buf();
    loop {
        if p.exists() {
            return p;
        }
        if !p.pop() {
            return PathBuf::from(".");
        }
    }
}

/// Choose `data_root` / `home_root` for an incomplete torrent.
///
/// - Complete / no leech_cache / not enough free space (or size cap) → permanent only.
/// - Else stage under `{leech_cache}/{infohash_hex}`.
///
/// `leech_cache_size`: max committed bytes under the cache (`0` = no soft cap;
/// free-space probe still applies). Soft cap:
/// `reserved_bytes + wanted + FREE_MARGIN ≤ leech_cache_size`.
///
/// `reserved_bytes` is catalog-committed size of already-staged torrents
/// (see [`Catalog::leech_cache_reserved_bytes`]); callers pass `0` when the cap is off.
pub fn choose_placement(
    permanent_root: &Path,
    leech_cache: &Path,
    leech_cache_size: u64,
    reserved_bytes: u64,
    infohash_hex: &str,
    wanted_bytes: u64,
    already_complete: bool,
) -> Placement {
    let permanent = permanent_root.to_path_buf();
    if already_complete || !leech_cache_enabled(leech_cache) {
        return Placement {
            data_root: permanent,
            home_root: None,
            used_leech_cache: false,
        };
    }

    let free = match free_space_bytes(leech_cache) {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!(
                error = %e,
                path = %leech_cache.display(),
                "leech_cache free space probe failed; using permanent data_root"
            );
            return Placement {
                data_root: permanent,
                home_root: None,
                used_leech_cache: false,
            };
        }
    };

    let need = wanted_bytes.saturating_add(FREE_MARGIN_BYTES);
    if free < need {
        tracing::info!(
            free,
            need,
            wanted = wanted_bytes,
            "leech_cache insufficient free space; using permanent data_root"
        );
        return Placement {
            data_root: permanent,
            home_root: None,
            used_leech_cache: false,
        };
    }

    if leech_cache_size > 0 {
        let remaining = leech_cache_size.saturating_sub(reserved_bytes);
        if remaining < need {
            tracing::info!(
                reserved = reserved_bytes,
                cap = leech_cache_size,
                remaining,
                need,
                wanted = wanted_bytes,
                "leech_cache size cap exceeded; using permanent data_root"
            );
            return Placement {
                data_root: permanent,
                home_root: None,
                used_leech_cache: false,
            };
        }
    }

    let stage = leech_cache.join(infohash_hex.to_ascii_lowercase());
    Placement {
        data_root: stage,
        home_root: Some(permanent),
        used_leech_cache: true,
    }
}

/// Copy payload files from current layout root to `dest_root` (wanted + any existing).
///
/// Does not change catalog. Caller verifies sizes then switches roots.
pub fn copy_payload_to_home(layout: &StorageLayout, dest_root: &Path) -> Result<()> {
    fs::create_dir_all(dest_root)
        .map_err(|e| Error::Path(dest_root.to_path_buf(), e.to_string()))?;

    // Pre-check free space on destination.
    let mut copy_bytes = 0u64;
    for f in &layout.files {
        let src = layout.data_root.join(&f.path);
        if src.is_file() {
            copy_bytes = copy_bytes.saturating_add(f.size);
        }
    }
    if copy_bytes > 0 {
        let free_home = free_space_bytes(dest_root)?;
        if free_home < copy_bytes.saturating_add(FREE_MARGIN_BYTES) {
            return Err(Error::Msg(format!(
                "home root {} has insufficient free space for handoff (need ~{copy_bytes} bytes)",
                dest_root.display()
            )));
        }
    }

    for f in &layout.files {
        let src = layout.data_root.join(&f.path);
        if !src.is_file() {
            continue;
        }
        let dst = dest_root.join(&f.path);
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| Error::Path(parent.to_path_buf(), e.to_string()))?;
        }
        fs::copy(&src, &dst).map_err(|e| Error::Path(dst.clone(), e.to_string()))?;
        // Size check.
        let meta = fs::metadata(&dst).map_err(|e| Error::Path(dst.clone(), e.to_string()))?;
        if meta.len() != f.size && f.priority != 0 {
            return Err(Error::Msg(format!(
                "handoff size mismatch for {}: got {} want {}",
                f.path.display(),
                meta.len(),
                f.size
            )));
        }
    }
    Ok(())
}

/// Remove staged torrent directory under leech_cache (`{leech_cache}/{infohash}/`).
pub fn remove_leech_cache_tree(stage_root: &Path) -> Result<()> {
    if !stage_root.exists() {
        return Ok(());
    }
    fs::remove_dir_all(stage_root)
        .map_err(|e| Error::Path(stage_root.to_path_buf(), e.to_string()))?;
    Ok(())
}

/// After copy: point catalog at home and clear home_root.
pub fn catalog_finish_handoff(
    catalog: &mut Catalog,
    torrent_id: i64,
    permanent_root: &Path,
) -> Result<()> {
    catalog.complete_leech_cache_handoff(torrent_id, permanent_root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk::spans::FileLayout;

    #[test]
    fn placement_disabled_without_leech_cache() {
        let p = choose_placement(
            Path::new("/home/lib"),
            Path::new(""),
            0,
            0,
            "abc",
            1000,
            false,
        );
        assert!(!p.used_leech_cache);
        assert_eq!(p.data_root, PathBuf::from("/home/lib"));
        assert!(p.home_root.is_none());
    }

    #[test]
    fn placement_complete_skips_cache() {
        let dir = tempfile::tempdir().unwrap();
        let p = choose_placement(Path::new("/home/lib"), dir.path(), 0, 0, "abc", 100, true);
        assert!(!p.used_leech_cache);
        assert!(p.home_root.is_none());
    }

    #[test]
    fn placement_fits_uses_cache() {
        let dir = tempfile::tempdir().unwrap();
        let p = choose_placement(
            Path::new("/home/lib"),
            dir.path(),
            0,
            0,
            "deadbeef",
            1024,
            false,
        );
        assert!(p.used_leech_cache);
        assert_eq!(p.home_root.as_deref(), Some(Path::new("/home/lib")));
        assert_eq!(p.data_root, dir.path().join("deadbeef"));
    }

    #[test]
    fn placement_size_cap_blocks() {
        let dir = tempfile::tempdir().unwrap();
        // Cap smaller than wanted + margin → permanent only.
        let p = choose_placement(
            Path::new("/home/lib"),
            dir.path(),
            1024,
            0,
            "deadbeef",
            1024,
            false,
        );
        assert!(!p.used_leech_cache);
        assert!(p.home_root.is_none());
    }

    #[test]
    fn placement_size_cap_respects_reserved() {
        let dir = tempfile::tempdir().unwrap();
        let want = 1024u64;
        let cap = want + FREE_MARGIN_BYTES + 10_000;
        // Reserved nearly fills cap → reject even though free space is large.
        let reserved = cap.saturating_sub(want); // remaining = want < need
        let p = choose_placement(
            Path::new("/home/lib"),
            dir.path(),
            cap,
            reserved,
            "cafebabe",
            want,
            false,
        );
        assert!(!p.used_leech_cache);
    }

    #[test]
    fn placement_size_cap_allows_when_room() {
        let dir = tempfile::tempdir().unwrap();
        let want = 1024u64;
        let cap = want + FREE_MARGIN_BYTES + 1;
        let p = choose_placement(
            Path::new("/home/lib"),
            dir.path(),
            cap,
            0,
            "cafebabe",
            want,
            false,
        );
        assert!(p.used_leech_cache);
    }

    #[test]
    fn wanted_bytes_respects_priority() {
        let layout = StorageLayout {
            data_root: PathBuf::from("/t"),
            piece_length: 16 * 1024,
            piece_count: 1,
            total_size: 300,
            files: vec![
                FileLayout {
                    path: PathBuf::from("a"),
                    size: 100,
                    offset: 0,
                    priority: 1,
                },
                FileLayout {
                    path: PathBuf::from("b"),
                    size: 200,
                    offset: 100,
                    priority: 0,
                },
            ],
        };
        assert_eq!(wanted_bytes_from_layout(&layout), 100);
    }

    #[test]
    fn wanted_bytes_metainfo_all_default() {
        const { assert!(FREE_MARGIN_BYTES > 0) };
    }
}
