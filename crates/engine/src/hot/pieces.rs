//! Piece possession, wanted set, claims, availability, rarest-first pick.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::catalog::{all_set_bitfield, bitfield_get, bitfield_set, empty_bitfield};
use crate::disk::StorageLayout;
use crate::error::{Error, Result};

use super::HotTorrent;

impl HotTorrent {
    pub fn has_piece(&self, index: u32) -> bool {
        if index >= self.piece_count {
            return false;
        }
        let st = self.pieces.read();
        if st.complete {
            return true;
        }
        bitfield_get(&st.bitfield, index)
    }

    /// Whether the piece intersects any file with priority > 0. O(1).
    pub fn wants_piece(&self, index: u32) -> bool {
        if index >= self.piece_count {
            return false;
        }
        bitfield_get(&self.wanted_bf.read(), index)
    }

    /// Mark pieces covered by priority>0 files using **file byte ranges**
    /// (O(files + pieces_in_wanted_ranges)), not O(pieces × files).
    fn build_wanted_bitfield(layout: &StorageLayout, prios: &[i32]) -> Vec<u8> {
        let pc = layout.piece_count;
        let mut bf = empty_bitfield(pc);
        if pc == 0 || layout.piece_length == 0 {
            return bf;
        }
        let plen = layout.piece_length as u64;
        let last = pc - 1;
        for (i, f) in layout.files.iter().enumerate() {
            let p = prios.get(i).copied().unwrap_or(f.priority);
            if p <= 0 || f.size == 0 {
                continue;
            }
            let start = (f.offset / plen) as u32;
            let end_off = f.offset.saturating_add(f.size).saturating_sub(1);
            let end = (end_off / plen) as u32;
            let start = start.min(last);
            let end = end.min(last);
            for idx in start..=end {
                bitfield_set(&mut bf, idx);
            }
        }
        bf
    }

    /// Rebuild wanted bitfield + missing counter (load / priority change).
    ///
    /// Tracker/UI `left` is full-torrent (`left_bytes`); only dial/leech stop
    /// uses priority-filtered `download_missing`.
    pub fn rebuild_wanted_and_missing(&self) {
        let prios = self.file_priority.read().clone();
        let wanted = Self::build_wanted_bitfield(&self.layout(), &prios);
        // Clone under pieces lock, then drop before wanted_bf write — never hold
        // pieces.read across another hot lock (fair RwLock + mark_have writers).
        let (complete, have_bf) = {
            let st = self.pieces.read();
            if st.complete {
                (true, None)
            } else {
                (false, Some(st.bitfield.clone()))
            }
        };
        if complete {
            *self.wanted_bf.write() = wanted;
            self.download_missing.store(0, Ordering::Relaxed);
            return;
        }
        let have_bf = have_bf.expect("incomplete ⇒ bitfield snapshot");
        let mut n = 0u32;
        for i in 0..self.piece_count {
            if bitfield_get(&wanted, i) && !bitfield_get(&have_bf, i) {
                n += 1;
            }
        }
        *self.wanted_bf.write() = wanted;
        self.download_missing.store(n, Ordering::Relaxed);
    }

    /// Recompute wanted-missing after priority change (alias for callers).
    pub fn recount_download_missing(&self) {
        self.rebuild_wanted_and_missing();
    }

    /// Update one file's priority live (catalog already written).
    /// Recounts wanted-missing so dial/leech stop when only off-file pieces remain.
    pub fn set_file_priority(&self, file_idx: u32, priority: i32) {
        {
            let mut prios = self.file_priority.write();
            if (file_idx as usize) < prios.len() {
                prios[file_idx as usize] = priority;
            } else if (file_idx as usize) < self.layout().files.len() {
                prios.resize(self.layout().files.len(), 1);
                prios[file_idx as usize] = priority;
            }
        }
        self.rebuild_wanted_and_missing();
        if self.is_download_complete() {
            self.release_staging_pool();
        } else {
            self.ensure_staging_pool();
        }
    }

    /// Full torrent (every piece). Off files may keep this false forever.
    pub fn is_complete(&self) -> bool {
        self.pieces.read().complete
    }

    /// All **wanted** pieces present — file priority 0 gaps do not count.
    ///
    /// This is what should stop outbound leech dials and download loops.
    /// **O(1)** — safe on every TUI snapshot tick.
    pub fn is_download_complete(&self) -> bool {
        self.download_missing.load(Ordering::Relaxed) == 0
    }

    /// Whether to allow multi piece claim on the last stretch (endgame).
    ///
    /// True when few wanted pieces remain, or every remaining piece is already
    /// claimed (nothing left to start alone). Enabling endgame only flips claim
    /// policy to multi-source so peers can race the same indices. It does not
    /// cancel or re-issue outstanding block Requests.
    ///
    /// Not gated on peer/seed count.
    pub fn should_endgame(&self) -> bool {
        let missing = self.missing_piece_count();
        if missing == 0 {
            return false;
        }
        // Keep this tight: early multi-source burns download bytes (hurts ratio)
        // and competes with upload bandwidth. Race only the last handful.
        const ENDGAME_MAX_MISSING: u32 = 8;
        if missing <= ENDGAME_MAX_MISSING {
            return true;
        }
        // Exclusive claims cover all remaining work → multi-source or stall.
        let claimed = self.in_flight.read().len() as u32;
        claimed >= missing
    }

    /// Full-torrent have count. Lock-free (safe during concurrent `mark_have`).
    pub fn have_count(&self) -> u32 {
        self.have_count_atomic.load(Ordering::Relaxed)
    }

    /// Bytes still needed for the **full** torrent (tracker `left`, UI).
    ///
    /// Full-torrent remaining = `total_size - completed_bytes` (libtorrent-compatible
    /// tracker `left`). File priority does not shrink this: skipped/off files still
    /// count until those pieces are complete. Dial/leech stop uses
    /// [`Self::is_download_complete`] (wanted-only) instead.
    pub fn left_bytes(&self) -> u64 {
        self.layout()
            .total_size
            .saturating_sub(self.completed_bytes())
    }

    /// Payload bytes covered by have pieces (full torrent, not priority-filtered).
    /// Matches rtorrent `file_list()->completed_bytes()` for tracker `downloaded`.
    pub fn completed_bytes(&self) -> u64 {
        self.completed_payload.load(Ordering::Relaxed)
    }

    /// Recompute full-torrent completed payload from the have bitfield.
    pub(super) fn recount_completed_payload(&self) {
        let st = self.pieces.read();
        if st.complete {
            self.completed_payload
                .store(self.layout().total_size, Ordering::Relaxed);
            return;
        }
        let mut n = 0u64;
        for i in 0..self.piece_count {
            if bitfield_get(&st.bitfield, i) {
                n += self.layout().piece_size(i).unwrap_or(0) as u64;
            }
        }
        self.completed_payload.store(n, Ordering::Relaxed);
    }

    pub fn bitfield_snapshot(&self) -> Vec<u8> {
        let st = self.pieces.read();
        if st.complete {
            all_set_bitfield(self.piece_count)
        } else {
            st.bitfield.clone()
        }
    }

    /// Clone of piece hash (20 bytes). Hashes may have been freed after full complete.
    pub fn piece_hash(&self, index: u32) -> Result<Vec<u8>> {
        let hashes = self.piece_hashes.read().clone();
        let start = index as usize * 20;
        let end = start + 20;
        if end > hashes.len() {
            return Err(Error::Msg(
                "piece hash OOB (or hashes released after complete)".into(),
            ));
        }
        Ok(hashes[start..end].to_vec())
    }

    /// Release piece-hash table once the full torrent is complete (seed-only needs bitfield).
    pub fn drop_piece_hashes_if_complete(&self) {
        if self.is_complete() {
            let mut h = self.piece_hashes.write();
            if !h.is_empty() {
                *h = Arc::new(Vec::new());
            }
        }
    }

    /// Try to reserve `index` for download by this peer.
    ///
    /// Outside endgame, claims are **exclusive** (one peer owns the piece until
    /// release/have). In `endgame`, multi-source is allowed so remaining pieces
    /// finish quickly.
    pub fn try_claim_piece(&self, index: u32, endgame: bool) -> bool {
        if index >= self.piece_count || self.has_piece(index) {
            return false;
        }
        if endgame {
            // Multi-source race; still record for endgame heuristics.
            self.in_flight.write().insert(index);
            return true;
        }
        self.in_flight.write().insert(index)
    }

    /// Drop exclusive reservation (peer exit, abandon, or after have).
    pub fn release_piece_claim(&self, index: u32) {
        self.in_flight.write().remove(&index);
    }

    // --- Piece availability (soft rarest-first) ---

    pub fn availability(&self, index: u32) -> u16 {
        self.availability
            .read()
            .get(index as usize)
            .copied()
            .unwrap_or(0)
    }

    pub fn availability_snapshot(&self) -> Vec<u16> {
        self.availability.read().clone()
    }

    pub fn avail_inc(&self, index: u32) {
        let mut a = self.availability.write();
        if let Some(c) = a.get_mut(index as usize) {
            *c = c.saturating_add(1);
        }
    }

    pub fn avail_dec(&self, index: u32) {
        let mut a = self.availability.write();
        if let Some(c) = a.get_mut(index as usize) {
            *c = c.saturating_sub(1);
        }
    }

    pub fn avail_add_bitfield(&self, bf: &[u8]) {
        let pc = self.piece_count;
        let mut a = self.availability.write();
        if a.len() < pc as usize {
            a.resize(pc as usize, 0);
        }
        for i in 0..pc {
            if bitfield_get(bf, i) {
                if let Some(c) = a.get_mut(i as usize) {
                    *c = c.saturating_add(1);
                }
            }
        }
    }

    pub fn avail_sub_bitfield(&self, bf: &[u8]) {
        let pc = self.piece_count;
        let mut a = self.availability.write();
        for i in 0..pc {
            if bitfield_get(bf, i) {
                if let Some(c) = a.get_mut(i as usize) {
                    *c = c.saturating_sub(1);
                }
            }
        }
    }

    /// Max random probes (large torrents) / exact-pass threshold (pc ≤ this).
    const PICK_SAMPLE_ATTEMPTS: u32 = 256;
    /// Max eligible candidates to rank by rarity then try to claim.
    const PICK_CANDIDATE_CAP: usize = 32;
    /// First N completes: random pick (classic BT) so we get uploadable pieces
    /// ASAP for tit-for-tat / ratio. Only on larger torrents.
    const INITIAL_RANDOM_PIECES: u32 = 4;
    /// Skip random-first when the torrent is smaller than this.
    const INITIAL_RANDOM_MIN_PC: u32 = 16;

    /// Soft rarest-first among pieces `peer_bf` has that we still want.
    ///
    /// Tuned for **seedbox ratio**: finish download fast (use every unchoked
    /// peer), start uploading complete pieces early, avoid mid-download
    /// multi-source waste.
    ///
    /// - Bootstrap: random among eligible (when few pieces have, large torrent)
    /// - Steady: prefer rarer pieces; **any** missing eligible piece is fine
    ///   if rare ones are claimed or absent from this peer
    /// - Multi-source only via `try_claim` endgame flag
    /// - Endgame: prefer pieces already in `in_flight` so idle unchoked peers
    ///   pile onto the same last pieces (aggressive multi-source)
    ///
    /// Still respects wanted files, staging, hashing, and `piece_ok`
    /// (e.g. Allowed Fast while choked).
    ///
    /// Returns `(piece_index, piece_length)`.
    pub fn pick_rarest_piece(
        &self,
        peer_bf: &[u8],
        in_staging: impl Fn(u32) -> bool,
        is_hashing: impl Fn(u32) -> bool,
        mut try_claim: impl FnMut(u32) -> bool,
        // Extra gate (BEP 6: while choked, only Allowed Fast indices).
        piece_ok: impl Fn(u32) -> bool,
        endgame: bool,
    ) -> Option<(u32, u32)> {
        use rand::RngExt;
        let pc = self.piece_count;
        if pc == 0 {
            return None;
        }

        // Clone-and-drop: copy hot bitfields under the locks, then release **before**
        // the eligible walk / sort / try_claim.
        //
        // Holding `pieces`+`wanted`+`availability` across the walk used to deadlock
        // when a helper re-took `pieces` under a waiting `mark_have` writer (fair
        // RwLock). Even without re-entry, long holds park peer I/O workers.
        // Bitfields are small; avail is O(piece_count) u16 — cheap vs transfer stall.
        let (complete, have_n, have_bf, wanted, avail, missing_n, inflight) = {
            let pieces = self.pieces.read();
            let wanted = self.wanted_bf.read();
            let avail = self.availability.read();
            // Snapshot in_flight only in endgame (prefer joining races).
            let inflight = if endgame {
                Some(self.in_flight.read().clone())
            } else {
                None
            };
            let complete = pieces.complete;
            let have_n = pieces.have_count;
            let have_bf = if complete {
                None
            } else {
                Some(pieces.bitfield.clone())
            };
            let wanted = wanted.clone();
            let avail = avail.clone();
            // Atomic — do not call missing_piece_count helpers that re-lock pieces.
            let missing_n = if complete {
                0
            } else {
                self.download_missing.load(Ordering::Relaxed)
            };
            (
                complete, have_n, have_bf, wanted, avail, missing_n, inflight,
            )
        };

        let eligible = |i: u32| -> bool {
            if i >= pc || is_hashing(i) || in_staging(i) || !piece_ok(i) {
                return false;
            }
            if complete {
                return false;
            }
            if let Some(ref h) = have_bf {
                if bitfield_get(h, i) {
                    return false;
                }
            }
            if !bitfield_get(&wanted, i) {
                return false;
            }
            bitfield_get(peer_bf, i)
        };

        let rarity = |i: u32| -> u16 { avail.get(i as usize).copied().unwrap_or(0) };

        let mut rng = rand::rng();
        // (index, rarity) — collect broad set, then claim rarest-first with fallback.
        let mut candidates: Vec<(u32, u16)> = Vec::with_capacity(Self::PICK_CANDIDATE_CAP);

        if pc <= Self::PICK_SAMPLE_ATTEMPTS {
            // Small torrent: exact pass from random start.
            let start = if pc > 1 { rng.random_range(0..pc) } else { 0 };
            for off in 0..pc {
                if candidates.len() >= Self::PICK_CANDIDATE_CAP {
                    break;
                }
                let i = (start + off) % pc;
                if eligible(i) {
                    candidates.push((i, rarity(i)));
                }
            }
        } else {
            // Large torrent: bounded random sample of eligible hits.
            for _ in 0..Self::PICK_SAMPLE_ATTEMPTS {
                if candidates.len() >= Self::PICK_CANDIDATE_CAP {
                    break;
                }
                let i = rng.random_range(0..pc);
                if eligible(i) && !candidates.iter().any(|(j, _)| *j == i) {
                    candidates.push((i, rarity(i)));
                }
            }
        }

        // Sparse remaining work, or unlucky sample: exact walk of **all** pieces.
        // A 256-probe sample + short walk misses the last 1–2 of 1024 ~30–60% of
        // the time; without a later fill event that becomes a permanent endgame stall.
        let exact = candidates.is_empty() || missing_n <= Self::PICK_CANDIDATE_CAP as u32;
        if exact {
            candidates.clear();
            let start = if pc > 1 { rng.random_range(0..pc) } else { 0 };
            for off in 0..pc {
                if candidates.len() >= Self::PICK_CANDIDATE_CAP {
                    break;
                }
                let i = (start + off) % pc;
                if eligible(i) {
                    candidates.push((i, rarity(i)));
                }
            }
        }

        // Bootstrap: random eligible → complete first pieces ASAP → upload.
        // Steady: soft rarest (rarer first, any missing is fine as fallback).
        // Endgame: join existing races first (in_flight), then rarest.
        let random_first = !endgame
            && have_n < Self::INITIAL_RANDOM_PIECES
            && pc >= Self::INITIAL_RANDOM_MIN_PC
            && missing_n > Self::PICK_CANDIDATE_CAP as u32;
        if endgame {
            candidates.sort_by(|a, b| {
                let a_race = inflight.as_ref().map(|s| s.contains(&a.0)).unwrap_or(false);
                let b_race = inflight.as_ref().map(|s| s.contains(&b.0)).unwrap_or(false);
                // true (already racing) sorts before false
                b_race.cmp(&a_race).then_with(|| a.1.cmp(&b.1)) // then rarer (lower avail)
            });
        } else if !random_first {
            candidates.sort_by_key(|(_, r)| *r);
        }

        // Locks already dropped; try_claim may take in_flight / pieces.
        for (i, _) in candidates {
            if !try_claim(i) {
                continue;
            }
            let plen = self.layout().piece_size(i).ok()?;
            return Some((i, plen));
        }
        None
    }
}
