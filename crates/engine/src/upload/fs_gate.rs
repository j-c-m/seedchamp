//! Platform gate for Compio seed fill under `upload.backend=auto`.
//!
//! On Linux, Compio `read_at` goes through io_uring. That is fast on page-cache
//! FSes (ext4/xfs/btrfs) but often **slower** than pread on ZFS/tmpfs for small
//! cached reads. [`path_allows_compio_fill`] gates `auto` accordingly.
//!
//! On Darwin, Compio fill is slower than blocking pread for typical seed
//! blocks, so `auto` uses **pread**. FreeBSD keeps Compio for `auto`.
//!
//! Force Compio on any FS: `upload.backend=compio`, or for Linux `auto`
//! `SEEDCHAMP_UPLOAD_COMPIO_FS=all` (or any/force/1/true/on).

use crate::disk::spans::IoSpan;
use crate::upload::ResolvedUploadBackend;

/// True when fill should use Compio `read_at` for these spans under `backend`.
///
/// - [`ResolvedUploadBackend::Pread`]: always false  
/// - [`ResolvedUploadBackend::Compio`]: always true  
/// - [`ResolvedUploadBackend::Auto`]: Linux FS gate; Darwin pread; FreeBSD Compio  
pub fn prefer_compio_fill(backend: ResolvedUploadBackend, spans: &[IoSpan]) -> bool {
    match backend {
        ResolvedUploadBackend::Pread => false,
        ResolvedUploadBackend::Compio => true,
        ResolvedUploadBackend::Auto => {
            #[cfg(target_os = "linux")]
            {
                if upload_compio_any_fs() {
                    return true;
                }
                // Empty spans: no I/O — treat as allowed.
                if spans.is_empty() {
                    return true;
                }
                spans.iter().all(|s| path_allows_compio_fill(&s.path))
            }
            // Darwin: Compio asyncify-style fill is slower than pread for seed blocks.
            #[cfg(target_os = "macos")]
            {
                let _ = spans;
                false
            }
            // FreeBSD and other non-Linux: Compio `read_at` for `auto`.
            #[cfg(all(not(target_os = "linux"), not(target_os = "macos")))]
            {
                let _ = spans;
                true
            }
        }
    }
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)] // libc::statfs for f_type only
mod linux {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::sync::OnceLock;

    static ANY_FS: OnceLock<bool> = OnceLock::new();

    /// Allow Compio fill on any filesystem (skip ext4/xfs/btrfs gate).
    pub(super) fn upload_compio_any_fs() -> bool {
        *ANY_FS.get_or_init(|| env_truthy_any_fs("SEEDCHAMP_UPLOAD_COMPIO_FS"))
    }

    fn env_truthy_any_fs(key: &str) -> bool {
        match std::env::var(key) {
            Ok(v) => {
                let v = v.trim().to_ascii_lowercase();
                matches!(v.as_str(), "all" | "any" | "force" | "1" | "true" | "on")
            }
            Err(_) => false,
        }
    }

    // Linux `statfs.f_type` magics (man 2 statfs / linux/magic.h).
    const EXT4_SUPER_MAGIC: i64 = 0xEF53; // ext2/3/4
    const XFS_SUPER_MAGIC: i64 = 0x5846_5342; // "XFSB"
    const BTRFS_SUPER_MAGIC: i64 = 0x9123_683E;

    /// True if this filesystem type is a good host for Compio/io_uring seed fill.
    #[inline]
    pub fn fs_type_allows_compio_fill(f_type: i64) -> bool {
        f_type == EXT4_SUPER_MAGIC || f_type == XFS_SUPER_MAGIC || f_type == BTRFS_SUPER_MAGIC
    }

    /// Whether Compio fill should be used for reads under `path` (`auto` path).
    ///
    /// false on stat failure (caller falls back to pread).
    pub fn path_allows_compio_fill(path: &Path) -> bool {
        if upload_compio_any_fs() {
            return true;
        }
        match statfs_type(path) {
            Some(t) => fs_type_allows_compio_fill(t),
            None => false,
        }
    }

    fn resolve_existing_ancestor(path: &Path) -> PathBuf {
        let mut p = path.to_path_buf();
        loop {
            if p.as_os_str().is_empty() {
                return PathBuf::from(".");
            }
            if p.exists() {
                return p;
            }
            if !p.pop() {
                return PathBuf::from(".");
            }
        }
    }

    fn statfs_type(path: &Path) -> Option<i64> {
        thread_local! {
            static PATH_TYPE: RefCell<HashMap<PathBuf, i64>> = RefCell::new(HashMap::new());
            static DEV_TYPE: RefCell<HashMap<u64, i64>> = RefCell::new(HashMap::new());
        }

        let probe = resolve_existing_ancestor(path);

        if let Some(t) = PATH_TYPE.with(|c| c.borrow().get(&probe).copied()) {
            return Some(t);
        }
        if let Ok(meta) = std::fs::metadata(&probe) {
            use std::os::unix::fs::MetadataExt;
            let dev = meta.dev();
            if let Some(t) = DEV_TYPE.with(|c| c.borrow().get(&dev).copied()) {
                PATH_TYPE.with(|c| {
                    c.borrow_mut().insert(probe.clone(), t);
                });
                return Some(t);
            }
        }

        let cpath = CString::new(probe.as_os_str().as_bytes()).ok()?;
        let mut st: libc::statfs = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::statfs(cpath.as_ptr(), &mut st) };
        if rc != 0 {
            return None;
        }
        let f_type = st.f_type as i64;
        PATH_TYPE.with(|c| {
            c.borrow_mut().insert(probe.clone(), f_type);
        });
        if let Ok(meta) = std::fs::metadata(&probe) {
            use std::os::unix::fs::MetadataExt;
            DEV_TYPE.with(|c| {
                c.borrow_mut().insert(meta.dev(), f_type);
            });
        }
        Some(f_type)
    }

    #[cfg(test)]
    mod tests {
        use super::fs_type_allows_compio_fill;
        use crate::upload::fs_gate::prefer_compio_fill;
        use crate::upload::ResolvedUploadBackend;

        #[test]
        fn magics_allow_list() {
            assert!(fs_type_allows_compio_fill(0xEF53)); // ext4
            assert!(fs_type_allows_compio_fill(0x5846_5342)); // xfs
            assert!(fs_type_allows_compio_fill(0x9123_683E)); // btrfs
                                                              // ZFS on Linux is typically 0x2FC12FC1
            assert!(!fs_type_allows_compio_fill(0x2FC1_2FC1));
            assert!(!fs_type_allows_compio_fill(0x0102_1994)); // tmpfs
        }

        #[test]
        fn prefer_pread_backend() {
            assert!(!prefer_compio_fill(ResolvedUploadBackend::Pread, &[]));
        }

        #[test]
        fn prefer_compio_backend() {
            assert!(prefer_compio_fill(ResolvedUploadBackend::Compio, &[]));
        }
    }
}

#[cfg(target_os = "linux")]
use linux::{path_allows_compio_fill, upload_compio_any_fs};

#[cfg(test)]
#[cfg(target_os = "macos")]
mod tests_macos {
    use super::*;

    #[test]
    fn auto_prefers_pread_on_macos() {
        assert!(!prefer_compio_fill(ResolvedUploadBackend::Auto, &[]));
        assert!(prefer_compio_fill(ResolvedUploadBackend::Compio, &[]));
        assert!(!prefer_compio_fill(ResolvedUploadBackend::Pread, &[]));
    }
}

#[cfg(test)]
#[cfg(all(not(target_os = "linux"), not(target_os = "macos")))]
mod tests_non_linux {
    use super::*;

    #[test]
    fn auto_prefers_compio_off_linux() {
        assert!(prefer_compio_fill(ResolvedUploadBackend::Auto, &[]));
    }
}
