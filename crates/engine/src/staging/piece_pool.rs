//! Shared per-torrent freelist of piece-sized buffers (byte budget).
//!
//! **Lazy allocate** up to `capacity`: first [`try_acquire`]s may `vec!` once;
//! [`release`] returns buffers to the freelist so later acquires do not allocate.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;

/// Default leech staging RAM per torrent (256 MiB).
pub const DEFAULT_STAGING_MEM_LIMIT: u64 = 256 * 1024 * 1024;
/// Minimum piece buffers in a pool (assemble + hash overlap).
pub const MIN_PIECE_BUFFERS: usize = 2;
/// Safety cap on buffer count (tiny piece_length).
pub const MAX_PIECE_BUFFERS: usize = 4096;
/// Min seconds between "staging full" warnings (per pool / torrent).
const BUDGET_FULL_WARN_SECS: u64 = 5;

/// Compute how many piece buffers fit in `limit_bytes`.
pub fn buffer_count_for_limit(piece_length: u32, limit_bytes: u64) -> usize {
    let plen = (piece_length.max(1)) as u64;
    let limit = if limit_bytes == 0 {
        DEFAULT_STAGING_MEM_LIMIT
    } else {
        limit_bytes
    };
    let floor = plen.saturating_mul(MIN_PIECE_BUFFERS as u64);
    let limit = limit.max(floor);
    let n = (limit / plen) as usize;
    n.clamp(MIN_PIECE_BUFFERS, MAX_PIECE_BUFFERS)
}

#[derive(Debug)]
struct Inner {
    /// Recycled buffers (no alloc on reuse).
    free: Vec<Vec<u8>>,
    /// Buffers currently checked out via [`PieceBufferPool::try_acquire`].
    outstanding: usize,
}

/// Per-torrent piece buffer freelist (lazy growth to capacity).
#[derive(Debug)]
pub struct PieceBufferPool {
    /// Catalog torrent id (0 in unit tests).
    torrent_id: i64,
    /// Display name for logs (may be empty).
    torrent_name: String,
    piece_length: u32,
    limit_bytes: u64,
    capacity: usize,
    inner: Mutex<Inner>,
    /// Unix seconds of last budget-full warning (rate limit).
    last_full_warn_secs: AtomicU64,
}

impl PieceBufferPool {
    /// Create a pool that may hold up to `N = f(limit)` buffers.
    ///
    /// No piece buffers are allocated until the first [`try_acquire`].
    pub fn new(piece_length: u32, limit_bytes: u64) -> Self {
        Self::with_torrent(0, "", piece_length, limit_bytes)
    }

    /// Same as [`Self::new`] with torrent identity for wait-on-budget warnings.
    pub fn with_torrent(
        torrent_id: i64,
        torrent_name: impl Into<String>,
        piece_length: u32,
        limit_bytes: u64,
    ) -> Self {
        let piece_length = piece_length.max(1);
        let limit_bytes = if limit_bytes == 0 {
            DEFAULT_STAGING_MEM_LIMIT
        } else {
            limit_bytes
        };
        let capacity = buffer_count_for_limit(piece_length, limit_bytes);
        Self {
            torrent_id,
            torrent_name: torrent_name.into(),
            piece_length,
            limit_bytes,
            capacity,
            inner: Mutex::new(Inner {
                free: Vec::with_capacity(capacity.min(64)),
                outstanding: 0,
            }),
            last_full_warn_secs: AtomicU64::new(0),
        }
    }

    pub fn piece_length(&self) -> u32 {
        self.piece_length
    }

    pub fn limit_bytes(&self) -> u64 {
        self.limit_bytes
    }

    /// Max buffers this pool may ever hold (budget / piece_length).
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// How many more [`try_acquire`] calls can succeed right now.
    pub fn available(&self) -> usize {
        let g = self.inner.lock();
        self.capacity.saturating_sub(g.outstanding)
    }

    /// Buffers on the freelist (allocated but not in use).
    pub fn freelist_len(&self) -> usize {
        self.inner.lock().free.len()
    }

    pub fn try_acquire(&self) -> Option<Vec<u8>> {
        let need = self.piece_length as usize;
        let mut g = self.inner.lock();
        if let Some(mut buf) = g.free.pop() {
            g.outstanding += 1;
            if buf.len() != need {
                buf.resize(need, 0);
            }
            return Some(buf);
        }
        if g.outstanding >= self.capacity {
            let outstanding = g.outstanding;
            drop(g);
            self.warn_budget_full(outstanding);
            return None;
        }
        g.outstanding += 1;
        // Drop lock before alloc? Keep lock to avoid races on outstanding; one
        // piece_length alloc is fine under mutex for rare growth path.
        Some(vec![0u8; need])
    }

    /// Rate-limited warn when a leech peer cannot start a piece (RAM budget).
    fn warn_budget_full(&self, outstanding: usize) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let prev = self.last_full_warn_secs.load(Ordering::Relaxed);
        if now.saturating_sub(prev) < BUDGET_FULL_WARN_SECS {
            return;
        }
        if self
            .last_full_warn_secs
            .compare_exchange(prev, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return;
        }
        tracing::warn!(
            id = self.torrent_id,
            torrent = %self.torrent_name,
            outstanding,
            capacity = self.capacity,
            limit_bytes = self.limit_bytes,
            piece_length = self.piece_length,
            "download waiting on staging memory — all piece buffers in use"
        );
    }

    /// Drop a checked-out buffer without parking it (peer abandon).
    pub fn discard(&self) {
        let mut g = self.inner.lock();
        if g.outstanding > 0 {
            g.outstanding -= 1;
        }
    }

    /// Return a buffer to the freelist (no heap free). Wrong size is resized.
    pub fn release(&self, mut buf: Vec<u8>) {
        let need = self.piece_length as usize;
        if buf.len() != need {
            buf.resize(need, 0);
        }
        let mut g = self.inner.lock();
        if g.outstanding > 0 {
            g.outstanding -= 1;
        }
        // Cap freelist: outstanding + free should not exceed capacity.
        if g.free.len() + g.outstanding < self.capacity {
            g.free.push(buf);
        }
        // else drop buf (orphan release after capacity shrink / replace)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_1m_and_16m() {
        let n1 = buffer_count_for_limit(1024 * 1024, 256 * 1024 * 1024);
        assert_eq!(n1, 256);
        let n16 = buffer_count_for_limit(16 * 1024 * 1024, 256 * 1024 * 1024);
        assert_eq!(n16, 16);
    }

    #[test]
    fn lazy_no_alloc_until_acquire() {
        let pool = PieceBufferPool::new(1024 * 1024, 4 * 1024 * 1024); // cap 4
        assert_eq!(pool.capacity(), 4);
        assert_eq!(pool.available(), 4);
        assert_eq!(pool.freelist_len(), 0);
    }

    #[test]
    fn acquire_release_roundtrip() {
        let pool = PieceBufferPool::new(1024 * 1024, 4 * 1024 * 1024); // 4 buffers
        assert_eq!(pool.capacity(), 4);
        assert_eq!(pool.available(), 4);
        let mut held = Vec::new();
        for _ in 0..4 {
            held.push(pool.try_acquire().expect("acquire"));
        }
        assert!(pool.try_acquire().is_none());
        assert_eq!(pool.available(), 0);
        assert_eq!(pool.freelist_len(), 0);
        for b in held {
            pool.release(b);
        }
        assert_eq!(pool.available(), 4);
        assert_eq!(pool.freelist_len(), 4);
        // Reuse — no new capacity growth path needed.
        let b = pool.try_acquire().unwrap();
        assert_eq!(pool.freelist_len(), 3);
        pool.release(b);
        assert_eq!(pool.freelist_len(), 4);
    }

    #[test]
    fn zero_limit_uses_default() {
        let n = buffer_count_for_limit(1024 * 1024, 0);
        assert_eq!(n, 256);
    }

    #[test]
    fn tiny_limit_still_min_two() {
        let n = buffer_count_for_limit(16 * 1024 * 1024, 1);
        assert_eq!(n, 2); // floor 2 × piece
    }
}
