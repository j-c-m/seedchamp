//! Mark have, interest scan, staging pool lifecycle.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::catalog::{all_set_bitfield, bitfield_get, bitfield_set};

use super::HotTorrent;

impl HotTorrent {
    /// Mark piece have in memory. Returns true if **full** torrent became complete.
    ///
    /// On a **new** have, notifies all [`Self::subscribe_have`] peers so they can
    /// send BEP3 HAVE on the wire (initial bitfield alone is not enough mid-leech).
    pub fn mark_have(&self, index: u32) -> bool {
        self.release_piece_claim(index);
        let wanted = self.wants_piece(index);
        let plen = self.layout().piece_size(index).unwrap_or(0) as u64;
        let mut st = self.pieces.write();
        if st.complete || bitfield_get(&st.bitfield, index) {
            return st.complete;
        }
        bitfield_set(&mut st.bitfield, index);
        st.have_count += 1;
        self.have_count_atomic
            .store(st.have_count, Ordering::Relaxed);
        self.completed_payload.fetch_add(plen, Ordering::Relaxed);
        let mut wanted_just_done = false;
        if wanted {
            let prev_missing =
                self.download_missing
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                        Some(n.saturating_sub(1))
                    });
            if matches!(prev_missing, Ok(1)) {
                wanted_just_done = true;
            }
        }
        let fully = st.have_count >= self.piece_count && self.piece_count > 0;
        if fully {
            st.complete = true;
            st.bitfield = all_set_bitfield(self.piece_count);
            st.have_count = self.piece_count;
            self.have_count_atomic
                .store(self.piece_count, Ordering::Relaxed);
            self.download_missing.store(0, Ordering::Relaxed);
            self.completed_payload
                .store(self.layout().total_size, Ordering::Relaxed);
        }
        drop(st);
        // Fan-out after releasing the piece lock (subscribers may re-enter hot state).
        self.have_hub.publish(index);
        if wanted_just_done && !fully {
            // Wanted set finished but off-files may still be missing (not full complete).
            tracing::info!(
                id = self.id,
                torrent = %self.name,
                "wanted download complete"
            );
        }
        if fully {
            // Free multi‑MB hash blob once we no longer need to verify downloads.
            self.drop_piece_hashes_if_complete();
        }
        if self.is_download_complete() {
            self.release_staging_pool();
        }
        fully
    }

    /// Configure staging RAM budget (bytes). `0` → default 256 MiB.
    pub fn set_staging_mem_limit(&self, limit_bytes: u64) {
        let lim = if limit_bytes == 0 {
            crate::staging::DEFAULT_STAGING_MEM_LIMIT
        } else {
            limit_bytes
        };
        self.staging_mem_limit.store(lim, Ordering::Relaxed);
    }

    pub fn staging_mem_limit(&self) -> u64 {
        self.staging_mem_limit.load(Ordering::Relaxed)
    }

    /// Ensure a shared piece-buffer freelist exists (leech). No-op if already present
    /// or download is already complete.
    pub fn ensure_staging_pool(&self) {
        if self.is_download_complete() {
            return;
        }
        let mut slot = self.staging_pool.write();
        if slot.is_some() {
            return;
        }
        let limit = self.staging_mem_limit.load(Ordering::Relaxed);
        let pool = Arc::new(crate::staging::PieceBufferPool::with_torrent(
            self.id,
            self.name.clone(),
            self.layout().piece_length,
            limit,
        ));
        tracing::debug!(
            id = self.id,
            capacity = pool.capacity(),
            limit_bytes = pool.limit_bytes(),
            piece_length = pool.piece_length(),
            "staging piece buffer pool ready (lazy alloc)"
        );
        *slot = Some(pool);
    }

    /// Drop freelist RAM (wanted-complete / seed-only). In-flight hash jobs keep their `Vec`.
    pub fn release_staging_pool(&self) {
        let mut slot = self.staging_pool.write();
        if slot.is_some() {
            tracing::debug!(id = self.id, "releasing staging piece buffer pool");
            *slot = None;
        }
    }

    pub fn staging_pool(&self) -> Option<Arc<crate::staging::PieceBufferPool>> {
        self.staging_pool.read().clone()
    }

    /// `(used, cap, limit_bytes)` for the shared leech pool. `None` if no pool.
    pub fn staging_fill(&self, nonblocking: bool) -> Option<(usize, usize, u64)> {
        let slot = if nonblocking {
            self.staging_pool.try_read()?
        } else {
            self.staging_pool.read()
        };
        let pool = slot.as_ref()?.clone();
        drop(slot);
        let cap = pool.capacity();
        let used = cap.saturating_sub(pool.available());
        Some((used, cap, pool.limit_bytes()))
    }

    /// Exclusive piece claims currently held (assembling / hashing).
    pub fn in_flight_count(&self, nonblocking: bool) -> usize {
        let g = if nonblocking {
            match self.in_flight.try_read() {
                Some(g) => g,
                None => return 0,
            }
        } else {
            self.in_flight.read()
        };
        g.len()
    }

    /// First missing **wanted** piece that `peer_has` reports, or None.
    pub fn next_interest_piece(&self, peer_has: &dyn Fn(u32) -> bool) -> Option<u32> {
        if self.is_download_complete() {
            return None;
        }
        // Clone each hot field under its own lock; no nested holds; no locks
        // across peer_has.
        let have_bf = {
            let st = self.pieces.read();
            if st.complete {
                return None;
            }
            st.bitfield.clone()
        };
        let wanted = self.wanted_bf.read().clone();
        (0..self.piece_count)
            .find(|&i| bitfield_get(&wanted, i) && !bitfield_get(&have_bf, i) && peer_has(i))
    }

    /// Missing **wanted** pieces (endgame / queue heuristics).
    ///
    /// Lock-free: reads only `download_missing`. Full-torrent complete always stores
    /// 0 there, so this matches the old `pieces.complete` short-circuit without
    /// taking the piece lock (safe to call while holding other hot locks).
    pub fn missing_piece_count(&self) -> u32 {
        self.download_missing.load(Ordering::Relaxed)
    }
}
