//! LRU open-file cache with optional idle eviction.
//!
//! Holds **std** `File`s for blocking pread/pwrite paths, and **Compio**
//! `fs::File`s (cloned `SharedFd`) for peer seed fill via `read_at`.
//!
//! **Peer I/O workers** (`seedchamp-io`): one [`FdCache`] per OS thread via
//! thread-local storage ([`with_peer_fd_cache`] / [`open_read_compio_peer`]).
//! Compio files are `!Send`, so the cache cannot live in `Arc` across workers.
//! Peers are pinned to a worker, so TLS is the natural share point.
//!
//! Disk/hash threads keep a private [`FdCache`] (not the peer TLS).

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use compio::fs::File as CompioFile;

use crate::error::{Error, Result};

thread_local! {
    /// Seed-fill open-file cache for the current peer I/O worker thread.
    static PEER_WORKER_FD_CACHE: RefCell<FdCache> = RefCell::new(FdCache::default_cache());
}

/// Run `f` with the peer-worker TLS cache (short critical section — no `.await`).
pub fn with_peer_fd_cache<R>(f: impl FnOnce(&mut FdCache) -> R) -> R {
    PEER_WORKER_FD_CACHE.with(|c| f(&mut c.borrow_mut()))
}

/// Compio open for peer seed fill: TLS cache, short borrow, open outside on miss.
pub async fn open_read_compio_peer(path: &Path) -> Result<CompioFile> {
    {
        let hit = with_peer_fd_cache(|cache| cache.compio_get(path));
        if let Some(f) = hit {
            return Ok(f);
        }
    }
    let file = CompioFile::open(path)
        .await
        .map_err(|e| Error::Path(path.to_path_buf(), e.to_string()))?;
    with_peer_fd_cache(|cache| cache.compio_insert(path, file.clone()));
    Ok(file)
}

struct Entry {
    file: File,
    last_used: Instant,
    /// Opened with write (R/W). Read-only entries are upgraded on first write.
    writable: bool,
}

struct CompioEntry {
    file: CompioFile,
    last_used: Instant,
}

/// Cache of open file handles keyed by absolute path.
pub struct FdCache {
    max_open: usize,
    idle: Duration,
    map: HashMap<PathBuf, Entry>,
    /// Peer Compio `read_at` handles (separate from std; `from_std` is private).
    compio: HashMap<PathBuf, CompioEntry>,
}

impl FdCache {
    pub fn new(max_open: usize, idle: Duration) -> Self {
        Self {
            max_open: max_open.max(1),
            idle,
            map: HashMap::new(),
            compio: HashMap::new(),
        }
    }

    pub fn default_cache() -> Self {
        // Similar spirit to rtorrent max open files + close_idle.
        Self::new(128, Duration::from_secs(60))
    }

    pub fn len(&self) -> usize {
        self.map.len() + self.compio.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty() && self.compio.is_empty()
    }

    /// Open or reuse a read-only file (or an existing R/W handle).
    pub fn open_read(&mut self, path: &Path) -> Result<&File> {
        self.open_with(path, false)
    }

    /// Lookup / bump Compio entry (clone of `SharedFd`). Used by TLS peer path.
    fn compio_get(&mut self, path: &Path) -> Option<CompioFile> {
        self.evict_idle_compio();
        let e = self.compio.get_mut(path)?;
        e.last_used = Instant::now();
        Some(e.file.clone())
    }

    /// Insert Compio file after open-outside-miss (or refresh if raced).
    fn compio_insert(&mut self, path: &Path, file: CompioFile) {
        self.evict_idle_compio();
        if let Some(e) = self.compio.get_mut(path) {
            e.last_used = Instant::now();
            // Prefer existing entry; drop the extra open.
            return;
        }
        while self.compio.len() >= self.max_open {
            self.evict_lru_compio();
        }
        self.compio.insert(
            path.to_path_buf(),
            CompioEntry {
                file,
                last_used: Instant::now(),
            },
        );
    }

    /// Open or reuse a read/write file (create parent dirs not included).
    ///
    /// **Reuses** a cached writable FD. Only re-opens when the path was previously
    /// cached read-only (upgrade) or not cached (avoids open/close per piece).
    pub fn open_write(&mut self, path: &Path) -> Result<&File> {
        self.evict_idle();
        if let Some(e) = self.map.get(path) {
            if e.writable {
                let e = self.map.get_mut(path).unwrap();
                e.last_used = Instant::now();
                return Ok(&e.file);
            }
            // Read-only handle — must reopen R/W.
            self.map.remove(path);
        }
        self.open_with(path, true)
    }

    /// Open or reuse a writable file and return a `dup`'d handle.
    ///
    /// For AIO / io_uring ops that need an owned FD outliving the cache borrow.
    pub fn open_write_cloned(&mut self, path: &Path) -> Result<File> {
        let file = self.open_write(path)?;
        file.try_clone()
            .map_err(|e| Error::Path(path.to_path_buf(), e.to_string()))
    }

    fn open_with(&mut self, path: &Path, write: bool) -> Result<&File> {
        self.evict_idle();
        if self.map.contains_key(path) {
            // Existing entry: upgrade needed only if write requested on RO cache.
            if write {
                let writable = self.map.get(path).map(|e| e.writable).unwrap_or(false);
                if !writable {
                    self.map.remove(path);
                } else {
                    let e = self.map.get_mut(path).unwrap();
                    e.last_used = Instant::now();
                    return Ok(&e.file);
                }
            } else {
                let e = self.map.get_mut(path).unwrap();
                e.last_used = Instant::now();
                return Ok(&e.file);
            }
        }
        while self.map.len() >= self.max_open {
            self.evict_lru();
        }
        let file = if write {
            OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(path)
        } else {
            OpenOptions::new().read(true).open(path)
        }
        .map_err(|e| Error::Path(path.to_path_buf(), e.to_string()))?;
        self.map.insert(
            path.to_path_buf(),
            Entry {
                file,
                last_used: Instant::now(),
                writable: write,
            },
        );
        Ok(&self.map.get(path).unwrap().file)
    }

    /// Close all cached descriptors.
    pub fn clear(&mut self) {
        self.map.clear();
        self.compio.clear();
    }

    fn evict_idle(&mut self) {
        if self.idle.is_zero() {
            return;
        }
        let now = Instant::now();
        self.map
            .retain(|_, e| now.duration_since(e.last_used) < self.idle);
    }

    fn evict_idle_compio(&mut self) {
        if self.idle.is_zero() {
            return;
        }
        let now = Instant::now();
        self.compio
            .retain(|_, e| now.duration_since(e.last_used) < self.idle);
    }

    fn evict_lru(&mut self) {
        let oldest = self
            .map
            .iter()
            .min_by_key(|(_, e)| e.last_used)
            .map(|(p, _)| p.clone());
        if let Some(p) = oldest {
            self.map.remove(&p);
        }
    }

    fn evict_lru_compio(&mut self) {
        let oldest = self
            .compio
            .iter()
            .min_by_key(|(_, e)| e.last_used)
            .map(|(p, _)| p.clone());
        if let Some(p) = oldest {
            self.compio.remove(&p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn reuses_handle() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f");
        std::fs::File::create(&p).unwrap().write_all(b"hi").unwrap();

        let mut c = FdCache::new(4, Duration::from_secs(60));
        let _ = c.open_read(&p).unwrap();
        assert_eq!(c.len(), 1);
        let _ = c.open_read(&p).unwrap();
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn open_write_reuses_writable_fd() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f");
        std::fs::write(&p, b"hi").unwrap();

        let mut c = FdCache::new(4, Duration::from_secs(60));
        let _ = c.open_write(&p).unwrap();
        assert_eq!(c.len(), 1);
        // Second write must not thrash open/close.
        let _ = c.open_write(&p).unwrap();
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn open_write_upgrades_readonly() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f");
        std::fs::write(&p, b"hi").unwrap();

        let mut c = FdCache::new(4, Duration::from_secs(60));
        let _ = c.open_read(&p).unwrap();
        assert_eq!(c.len(), 1);
        let _ = c.open_write(&p).unwrap();
        assert_eq!(c.len(), 1);
        let _ = c.open_write(&p).unwrap();
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn respects_max_open() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = FdCache::new(2, Duration::from_secs(3600));
        for i in 0..3 {
            let p = dir.path().join(format!("f{i}"));
            std::fs::write(&p, b"x").unwrap();
            c.open_read(&p).unwrap();
        }
        assert!(c.len() <= 2);
    }

    #[compio::test]
    async fn open_read_compio_peer_tls_reuses() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f");
        std::fs::write(&p, b"hi").unwrap();

        with_peer_fd_cache(|c| c.clear());
        let a = open_read_compio_peer(&p).await.unwrap();
        let b = open_read_compio_peer(&p).await.unwrap();
        let n = with_peer_fd_cache(|c| c.len());
        assert_eq!(n, 1);
        drop(a);
        drop(b);
        assert_eq!(with_peer_fd_cache(|c| c.len()), 1);
    }
}
