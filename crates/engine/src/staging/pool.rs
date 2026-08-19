//! Piece staging: assemble blocks in RAM.
//!
//! Per peer: assembling/hashing metadata; piece **bytes** from a shared
//! [`PieceBufferPool`] freelist (torrent `staging_mem_limit`).

use crate::error::{Error, Result};

/// Standard BT request block size.
pub const BLOCK_SIZE: u32 = 16 * 1024;

/// One piece being assembled (metadata only; buffer lives in the slot).
#[derive(Debug)]
struct ActivePiece {
    index: u32,
    length: u32,
    have: Vec<bool>,
    requested: Vec<bool>,
    endgame: bool,
}

impl ActivePiece {
    fn open(index: u32, length: u32, max_blocks: usize) -> Self {
        let nblocks = num_blocks(length);
        debug_assert!(nblocks <= max_blocks);
        let mut have = vec![false; max_blocks];
        let mut requested = vec![false; max_blocks];
        have.truncate(nblocks);
        requested.truncate(nblocks);
        // Reuse capacity of max_blocks without shrinking allocation below nblocks.
        // Above truncate is fine for short last piece; for full pieces nblocks==max_blocks.
        Self {
            index,
            length,
            have,
            requested,
            endgame: false,
        }
    }

    fn is_complete(&self) -> bool {
        self.have.iter().all(|&h| h)
    }

    fn set_endgame(&mut self, on: bool) {
        self.endgame = on;
    }

    fn requeue_missing(&mut self) {
        for (i, h) in self.have.iter().enumerate() {
            if !*h {
                self.requested[i] = false;
            }
        }
    }

    fn clear_request(&mut self, begin: u32, length: u32) -> bool {
        if !begin.is_multiple_of(BLOCK_SIZE) {
            return false;
        }
        let bi = (begin / BLOCK_SIZE) as usize;
        if bi >= self.have.len() || self.have[bi] || !self.requested[bi] {
            return false;
        }
        let expect = block_len(self.length, bi as u32);
        if length != expect {
            return false;
        }
        self.requested[bi] = false;
        true
    }

    fn next_request(&mut self) -> Option<(u32, u32)> {
        for i in 0..self.have.len() {
            if !self.have[i] && !self.requested[i] {
                self.requested[i] = true;
                let begin = i as u32 * BLOCK_SIZE;
                let len = block_len(self.length, i as u32);
                return Some((begin, len));
            }
        }
        None
    }

    fn outstanding_requests(&self) -> usize {
        self.have
            .iter()
            .zip(self.requested.iter())
            .filter(|(&h, &r)| !h && r)
            .count()
    }

    fn outstanding_blocks(&self) -> Vec<(u32, u32)> {
        let mut out = Vec::new();
        for i in 0..self.have.len() {
            if !self.have[i] && self.requested[i] {
                out.push((i as u32 * BLOCK_SIZE, block_len(self.length, i as u32)));
            }
        }
        out
    }

    fn ingest(&mut self, buf: &mut [u8], begin: u32, data: &[u8]) -> Result<bool> {
        let end_off = begin as usize + data.len();
        if end_off > buf.len() || end_off > self.length as usize {
            return Err(Error::Msg("block overflows piece buffer".into()));
        }
        self.mark_range_have(begin, data.len() as u32, buf.len() as u32)?;
        buf[begin as usize..end_off].copy_from_slice(data);
        Ok(self.is_complete())
    }

    /// Mark complete 16 KiB blocks covered by `[begin, begin+length)` (data already in buf).
    fn mark_range_have(&mut self, begin: u32, length: u32, buf_len: u32) -> Result<()> {
        if length == 0 {
            return Ok(());
        }
        if !begin.is_multiple_of(BLOCK_SIZE)
            && begin + length != self.length
            && begin / BLOCK_SIZE != (begin + length - 1) / BLOCK_SIZE
        {
            return Err(Error::Msg("unaligned multi-block piece data".into()));
        }
        let bi = (begin / BLOCK_SIZE) as usize;
        if bi >= self.have.len() {
            return Err(Error::Msg("block begin past piece".into()));
        }
        if begin + length > self.length || begin + length > buf_len {
            return Err(Error::Msg("block past piece end".into()));
        }
        let end = begin + length;
        let start_b = (begin / BLOCK_SIZE) as usize;
        let end_b = end.div_ceil(BLOCK_SIZE) as usize;
        for i in start_b..end_b.min(self.have.len()) {
            let b0 = i as u32 * BLOCK_SIZE;
            let b1 = b0 + block_len(self.length, i as u32);
            if begin <= b0 && end >= b1 {
                self.have[i] = true;
            }
        }
        Ok(())
    }
}

pub fn num_blocks(piece_len: u32) -> usize {
    if piece_len == 0 {
        return 0;
    }
    piece_len.div_ceil(BLOCK_SIZE) as usize
}

pub fn block_len(piece_len: u32, block_index: u32) -> u32 {
    let begin = block_index * BLOCK_SIZE;
    if begin >= piece_len {
        return 0;
    }
    (piece_len - begin).min(BLOCK_SIZE)
}

#[derive(Debug)]
enum Slot {
    /// Receiving blocks for `piece.index` (buffer from [`PieceBufferPool`]).
    Assembling { piece: ActivePiece, buf: Vec<u8> },
    /// Piece handed to hash/disk; buffer not in this slot (with [`HashJob`]).
    Hashing { index: u32 },
}

/// Per-peer leech assembly state; piece **bytes** come from a shared
/// [`PieceBufferPool`] (torrent freelist), not a fixed private prealloc.
#[derive(Debug)]
pub struct StagingPool {
    slots: Vec<Slot>,
    piece_length: u32,
    max_blocks: usize,
    /// Shared torrent freelist; `None` = pure seeder (no leech).
    pool: Option<std::sync::Arc<super::piece_pool::PieceBufferPool>>,
}

impl StagingPool {
    /// Pure seeder: no piece buffers.
    pub fn empty(piece_length: u32) -> Self {
        let piece_length = piece_length.max(1);
        Self {
            slots: Vec::new(),
            piece_length,
            max_blocks: num_blocks(piece_length),
            pool: None,
        }
    }

    /// Shared per-torrent freelist.
    pub fn from_pool(pool: std::sync::Arc<super::piece_pool::PieceBufferPool>) -> Self {
        let piece_length = pool.piece_length();
        Self {
            slots: Vec::new(),
            piece_length,
            max_blocks: num_blocks(piece_length),
            pool: Some(pool),
        }
    }

    pub fn piece_length(&self) -> u32 {
        self.piece_length
    }

    /// Freelist capacity (shared pool), or 0 if seeder.
    pub fn capacity(&self) -> usize {
        self.pool.as_ref().map(|p| p.capacity()).unwrap_or(0)
    }

    fn find_assembling_mut(&mut self, index: u32) -> Option<(&mut ActivePiece, &mut Vec<u8>)> {
        for s in &mut self.slots {
            if let Slot::Assembling { piece, buf } = s {
                if piece.index == index {
                    return Some((piece, buf));
                }
            }
        }
        None
    }

    /// Insert a piece only when starting requests (`take_requests`). Do **not**
    /// call from PIECE handlers — that would allow unsolicited peers to grow RAM.
    ///
    /// Returns `false` if the freelist is empty (budget full).
    pub fn try_start(&mut self, index: u32, length: u32) -> bool {
        if self.contains(index) {
            return true;
        }
        if length == 0 || length > self.piece_length {
            return false;
        }
        let Some(pool) = self.pool.as_ref() else {
            return false;
        };
        let Some(buf) = pool.try_acquire() else {
            return false;
        };
        let piece = ActivePiece::open(index, length, self.max_blocks);
        self.slots.push(Slot::Assembling { piece, buf });
        true
    }

    pub fn contains(&self, index: u32) -> bool {
        self.slots.iter().any(|s| match s {
            Slot::Assembling { piece, .. } => piece.index == index,
            Slot::Hashing { index: i } => *i == index,
        })
    }

    /// Ingest a block only if `index` is already staged (we issued Requests).
    pub fn ingest_if_staged(
        &mut self,
        index: u32,
        begin: u32,
        data: &[u8],
    ) -> Result<Option<bool>> {
        let Some((piece, buf)) = self.find_assembling_mut(index) else {
            return Ok(None);
        };
        Ok(Some(piece.ingest(buf, begin, data)?))
    }

    /// Piece complete: move buffer to caller and mark hashing-in-flight.
    ///
    /// `data` is the full piece buffer; use `plen` for hash/write extent.
    /// Caller must [`Self::reclaim`] after hash/disk.
    pub fn take_for_hash(&mut self, index: u32) -> Option<(u32, Vec<u8>)> {
        let pos = self
            .slots
            .iter()
            .position(|s| matches!(s, Slot::Assembling { piece, .. } if piece.index == index))?;
        let Slot::Assembling { piece, buf } = self.slots.swap_remove(pos) else {
            return None;
        };
        // Hash/disk owns `buf`. Free the staging slot so other peers can leech
        // while this piece verifies and writes (16 MiB × disk depth=32 is 512 MiB).
        if let Some(ref pool) = self.pool {
            pool.detach();
        }
        self.slots.push(Slot::Hashing { index });
        Some((piece.length, buf))
    }

    /// Recycle a buffer after hash/disk (already detached from the budget).
    pub fn reclaim(&mut self, index: u32, data: Vec<u8>) {
        self.slots
            .retain(|s| !matches!(s, Slot::Hashing { index: i } if *i == index));
        if let Some(ref pool) = self.pool {
            pool.donate(data);
        }
        // else drop data (no pool)
    }

    /// Drop assembling pieces (buffers back to freelist). Hashing stays until reclaim.
    pub fn clear(&mut self) {
        let mut kept = Vec::new();
        for s in std::mem::take(&mut self.slots) {
            match s {
                Slot::Assembling { buf, .. } => {
                    if let Some(ref pool) = self.pool {
                        pool.release(buf);
                    }
                }
                Slot::Hashing { index } => kept.push(Slot::Hashing { index }),
            }
        }
        self.slots = kept;
    }

    pub fn len(&self) -> usize {
        self.slots
            .iter()
            .filter(|s| matches!(s, Slot::Assembling { .. }))
            .count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn can_start_more(&self) -> bool {
        self.pool
            .as_ref()
            .map(|p| p.available() > 0)
            .unwrap_or(false)
    }

    pub fn free_slots(&self) -> usize {
        self.pool.as_ref().map(|p| p.available()).unwrap_or(0)
    }

    pub fn enable_endgame(&mut self) {
        for s in &mut self.slots {
            if let Slot::Assembling { piece, .. } = s {
                piece.set_endgame(true);
            }
        }
    }

    pub fn requeue_timed_out(&mut self) {
        for s in &mut self.slots {
            if let Slot::Assembling { piece, .. } = s {
                piece.requeue_missing();
            }
        }
    }

    pub fn total_outstanding(&self) -> usize {
        self.slots
            .iter()
            .map(|s| match s {
                Slot::Assembling { piece, .. } => piece.outstanding_requests(),
                _ => 0,
            })
            .sum()
    }

    pub fn outstanding_list(&self) -> Vec<(u32, u32, u32)> {
        let mut out = Vec::new();
        for s in &self.slots {
            if let Slot::Assembling { piece, .. } = s {
                for (begin, len) in piece.outstanding_blocks() {
                    out.push((piece.index, begin, len));
                }
            }
        }
        out
    }

    pub fn indices(&self) -> Vec<u32> {
        self.slots
            .iter()
            .filter_map(|s| match s {
                Slot::Assembling { piece, .. } => Some(piece.index),
                _ => None,
            })
            .collect()
    }

    /// Clear outstanding flag for one block after RejectRequest.
    pub fn clear_request(&mut self, index: u32, begin: u32, length: u32) -> bool {
        if let Some((piece, _)) = self.find_assembling_mut(index) {
            return piece.clear_request(begin, length);
        }
        false
    }

    pub const PIPE_REFILL_DIV: usize = 2;

    /// Queue block requests until `pipeline` are outstanding.
    ///
    /// When `endgame` is true, refill aggressively (no low-water early return) so
    /// idle unchoked peers pile onto multi-claimed last pieces immediately.
    ///
    /// `start_piece` must claim the index it returns. If `try_start` then
    /// fails (freelist race), `abort_start` releases that claim.
    pub fn take_requests(
        &mut self,
        pipeline: usize,
        endgame: bool,
        mut start_piece: impl FnMut(&Self) -> Option<(u32, u32)>,
        mut may_request_piece: impl FnMut(u32) -> bool,
        mut abort_start: impl FnMut(u32),
    ) -> Vec<(u32, u32, u32)> {
        let pipeline = pipeline.max(1);
        // Steady: only refill when outstanding drops to half pipeline.
        // Endgame: always try to fill to the cap (aggressive multi-source).
        if !endgame {
            let low_water = (pipeline / Self::PIPE_REFILL_DIV).max(1);
            if self.total_outstanding() > low_water {
                return Vec::new();
            }
        }
        let max_pieces = max_assembling_pieces(pipeline, self.piece_length, self.capacity());
        let mut out = Vec::with_capacity(pipeline.saturating_sub(self.total_outstanding()));
        let max_slots = self.slots.len();
        for _ in 0..(pipeline * 4 + max_slots + 8) {
            if self.total_outstanding() >= pipeline {
                break;
            }
            let mut queued_from_existing = false;
            let idxs = self.indices();
            for idx in idxs {
                if !may_request_piece(idx) {
                    continue;
                }
                while self.total_outstanding() < pipeline {
                    let Some((piece, _)) = self.find_assembling_mut(idx) else {
                        break;
                    };
                    let Some((begin, len)) = piece.next_request() else {
                        break;
                    };
                    out.push((idx, begin, len));
                    queued_from_existing = true;
                }
                if self.total_outstanding() >= pipeline {
                    break;
                }
            }
            if self.total_outstanding() >= pipeline {
                break;
            }
            if !self.can_start_more() || self.len() >= max_pieces {
                break;
            }
            let Some((index, plen)) = start_piece(self) else {
                break;
            };
            if self.contains(index) {
                break;
            }
            if !self.try_start(index, plen) {
                abort_start(index);
                break;
            }
            if !queued_from_existing {
                if let Some((piece, _)) = self.find_assembling_mut(index) {
                    if let Some((begin, len)) = piece.next_request() {
                        out.push((index, begin, len));
                        continue;
                    }
                }
                break;
            }
        }
        out
    }
}

/// Max assembling pieces one peer may hold (hash/write does not count).
///
/// Enough to fill `pipeline` blocks, but never more than `1/16` of the
/// shared freelist. Pieces ≥4 MiB cap at 2 — one 16 MiB piece is already
/// 1024 blocks; a 1 GiB pool then runs ~32 peers instead of a handful.
pub fn max_assembling_pieces(pipeline: usize, piece_length: u32, capacity: usize) -> usize {
    if capacity == 0 {
        return 0;
    }
    let blocks = num_blocks(piece_length).max(1);
    let for_pipe = pipeline.div_ceil(blocks).max(1);
    const MIN_PEER_SPREAD: usize = 16;
    let fair = capacity.div_ceil(MIN_PEER_SPREAD).max(1);
    let n = for_pipe.min(fair);
    // 4 MiB = 256 blocks. Two such pieces cover any realistic BDP.
    const LARGE_PIECE_BLOCKS: usize = 256;
    const LARGE_PIECE_MAX: usize = 2;
    if blocks >= LARGE_PIECE_BLOCKS {
        n.min(LARGE_PIECE_MAX)
    } else {
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk::spans::FileLayout;
    use crate::disk::{ensure_storage, read_piece, write_piece, FdCache, StorageLayout};
    use sha1::{Digest, Sha1};
    use std::path::PathBuf;
    use std::sync::Arc;

    fn test_staging(buffer_count: usize, piece_length: u32) -> StagingPool {
        let limit = (buffer_count as u64).saturating_mul(piece_length as u64);
        StagingPool::from_pool(Arc::new(super::super::piece_pool::PieceBufferPool::new(
            piece_length,
            limit,
        )))
    }

    #[test]
    fn assemble_verify_write() {
        let dir = tempfile::tempdir().unwrap();
        let len = BLOCK_SIZE as usize * 2 + 100;
        let data: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        let mut h = Sha1::new();
        h.update(&data);
        let digest = h.finalize();
        let layout = StorageLayout {
            data_root: dir.path().to_path_buf(),
            piece_length: len as u32,
            piece_count: 1,
            total_size: len as u64,
            files: vec![FileLayout {
                path: PathBuf::from("x"),
                size: len as u64,
                offset: 0,
                priority: 1,
            }],
        };
        ensure_storage(&layout).unwrap();

        let mut pool = test_staging(1, len as u32);
        assert!(pool.try_start(0, len as u32));
        assert_eq!(num_blocks(len as u32), 3);
        assert_eq!(
            pool.ingest_if_staged(0, 0, &data[0..BLOCK_SIZE as usize])
                .unwrap(),
            Some(false)
        );
        assert_eq!(
            pool.ingest_if_staged(
                0,
                BLOCK_SIZE,
                &data[BLOCK_SIZE as usize..2 * BLOCK_SIZE as usize]
            )
            .unwrap(),
            Some(false)
        );
        assert_eq!(
            pool.ingest_if_staged(0, 2 * BLOCK_SIZE, &data[2 * BLOCK_SIZE as usize..])
                .unwrap(),
            Some(true)
        );
        let (_plen, buf) = pool.take_for_hash(0).unwrap();
        let mut h2 = Sha1::new();
        h2.update(&buf[..len]);
        assert_eq!(h2.finalize().as_slice(), digest.as_slice());

        let mut cache = FdCache::default_cache();
        write_piece(&mut cache, &layout, 0, &buf[..len]).unwrap();
        let mut out = Vec::new();
        read_piece(&mut cache, &layout, 0, &mut out).unwrap();
        assert_eq!(out, data);
        pool.reclaim(0, buf);
    }

    #[test]
    fn reject_clears_only_one_block() {
        let len = BLOCK_SIZE * 3;
        let mut pool = test_staging(1, len);
        assert!(pool.try_start(0, len));
        let reqs = pool.take_requests(3, true, |_| None, |_| true, |_| {});
        assert_eq!(reqs.len(), 3);
        assert_eq!(pool.total_outstanding(), 3);
        assert!(pool.clear_request(0, BLOCK_SIZE, BLOCK_SIZE));
        assert_eq!(pool.total_outstanding(), 2);
        let again = pool.take_requests(3, true, |_| None, |_| true, |_| {});
        assert_eq!(again, vec![(0, BLOCK_SIZE, BLOCK_SIZE)]);
        assert!(pool
            .take_requests(3, true, |_| None, |_| true, |_| {})
            .is_empty());
        assert!(!pool.clear_request(0, 0, 1));
        assert_eq!(pool.total_outstanding(), 3);
    }

    #[test]
    fn unsolicited_piece_does_not_allocate_staging() {
        let plen = BLOCK_SIZE * 2;
        let mut pool = test_staging(4, plen);
        let data = vec![0u8; BLOCK_SIZE as usize];
        assert!(pool.ingest_if_staged(0, 0, &data).unwrap().is_none());
        assert!(pool.is_empty());
        assert!(pool.try_start(0, plen));
        assert_eq!(pool.ingest_if_staged(0, 0, &data).unwrap(), Some(false));
        assert_eq!(pool.len(), 1);
    }

    #[test]
    fn fixed_slots_no_start_when_full() {
        let plen = BLOCK_SIZE * 2;
        let mut pool = test_staging(2, plen);
        assert!(pool.try_start(0, plen));
        assert!(pool.try_start(1, plen));
        assert!(!pool.can_start_more());
        assert!(!pool.try_start(2, plen));
        // Complete + take leaves InFlight; still no Free.
        let data = vec![0xab; BLOCK_SIZE as usize];
        assert_eq!(pool.ingest_if_staged(0, 0, &data).unwrap(), Some(false));
        assert_eq!(
            pool.ingest_if_staged(0, BLOCK_SIZE, &data).unwrap(),
            Some(true)
        );
        let (_plen, buf) = pool.take_for_hash(0).unwrap();
        // Hash/disk no longer holds the staging slot — next piece can start.
        assert!(pool.can_start_more());
        assert!(pool.try_start(2, plen));
        pool.reclaim(0, buf);
    }

    #[test]
    fn pipeline_requests() {
        let len = BLOCK_SIZE * 2 + 100;
        let mut pool = test_staging(1, len);
        assert!(pool.try_start(0, len));
        let reqs = pool.take_requests(10, true, |_| None, |_| true, |_| {});
        assert_eq!(
            reqs,
            vec![
                (0, 0, BLOCK_SIZE),
                (0, BLOCK_SIZE, BLOCK_SIZE),
                (0, 2 * BLOCK_SIZE, 100),
            ]
        );
        assert!(pool
            .take_requests(10, true, |_| None, |_| true, |_| {})
            .is_empty());
        pool.enable_endgame();
        assert!(pool
            .take_requests(10, true, |_| None, |_| true, |_| {})
            .is_empty());
        pool.requeue_timed_out();
        let again = pool.take_requests(10, true, |_| None, |_| true, |_| {});
        assert_eq!(again[0], (0, 0, BLOCK_SIZE));
        assert_eq!(again[1].1, BLOCK_SIZE);
        assert_eq!(again.len(), 3);
        assert!(pool
            .take_requests(10, true, |_| None, |_| true, |_| {})
            .is_empty());
    }

    #[test]
    fn take_requests_fills_pipeline() {
        let plen = BLOCK_SIZE * 64;
        let mut pool = test_staging(4, plen);
        let mut started = 0u32;
        let reqs = pool.take_requests(
            32,
            false,
            |st| {
                if st.len() >= 2 {
                    return None;
                }
                let idx = started;
                started += 1;
                Some((idx, plen))
            },
            |_| true,
            |_| {},
        );
        assert_eq!(reqs.len(), 32, "should queue full pipeline in one shot");
        assert_eq!(pool.total_outstanding(), 32);
        let mut seen = std::collections::HashSet::new();
        for (i, b, l) in &reqs {
            assert!(seen.insert((*i, *b)), "duplicate request {i}:{b}");
            assert_eq!(*l, BLOCK_SIZE);
        }
        let more = pool.take_requests(32, false, |_| Some((99, plen)), |_| true, |_| {});
        assert!(more.is_empty());
    }

    #[test]
    fn take_requests_respects_may_request_piece() {
        let plen = BLOCK_SIZE * 4;
        let mut pool = test_staging(4, plen);
        assert!(pool.try_start(0, plen));
        assert!(pool.try_start(1, plen));
        let reqs = pool.take_requests(8, false, |_| None, |i| i == 1, |_| {});
        assert!(!reqs.is_empty());
        assert!(reqs.iter().all(|(i, _, _)| *i == 1));
        // Piece 0 staged but not allowed — no outstanding on it.
        let out0: usize = pool
            .outstanding_list()
            .iter()
            .filter(|(i, _, _)| *i == 0)
            .count();
        assert_eq!(out0, 0);
    }

    #[test]
    fn reclaim_restores_free_slot() {
        let plen = BLOCK_SIZE;
        let mut pool = test_staging(2, plen);
        assert_eq!(pool.capacity(), 2);
        assert!(pool.try_start(0, plen));
        assert_eq!(pool.free_slots(), 1);
        let data = vec![1u8; BLOCK_SIZE as usize];
        assert_eq!(pool.ingest_if_staged(0, 0, &data).unwrap(), Some(true));
        let (len, buf) = pool.take_for_hash(0).unwrap();
        assert_eq!(len, plen);
        // Detached from staging budget; hash/disk owns `buf`.
        assert_eq!(pool.free_slots(), 2);
        pool.reclaim(0, buf);
        assert_eq!(pool.free_slots(), 2);
        assert!(pool.try_start(1, plen));
    }

    #[test]
    fn assembling_cap_spreads_large_piece_budget() {
        // 16 MiB pieces, 256 MiB budget → 16 buffers. A 8192-block pipe
        // would otherwise start 8 pieces on one peer.
        assert_eq!(
            max_assembling_pieces(8192, 16 * 1024 * 1024, 16),
            1,
            "one 16MiB piece per peer so 16 peers can leech"
        );
        assert_eq!(max_assembling_pieces(32, 16 * 1024 * 1024, 16), 1);
        // 1 GiB / 16 MiB = 64 buffers. Large-piece cap 2 → ~32 peers.
        assert_eq!(
            max_assembling_pieces(8192, 16 * 1024 * 1024, 64),
            2,
            "1G staging + 16MiB pieces must not collapse onto a handful of peers"
        );
        // 4 MiB pieces, 64 buffers: fair 4 but large-piece cap 2.
        assert_eq!(max_assembling_pieces(8192, 4 * 1024 * 1024, 64), 2);
        // Small pieces: pipe needs 1, don't start extras.
        assert_eq!(max_assembling_pieces(16, BLOCK_SIZE * 64, 256), 1);
        assert_eq!(max_assembling_pieces(32, BLOCK_SIZE * 64, 4), 1);
        assert_eq!(max_assembling_pieces(32, BLOCK_SIZE, 0), 0);
    }

    #[test]
    fn take_requests_caps_pieces_per_peer() {
        let plen = BLOCK_SIZE * 4; // 4 blocks/piece
        let mut pool = test_staging(32, plen); // plenty of buffers
                                               // pipeline 64 would want 16 pieces; cap is fair share of 32 = 2.
        let mut next = 0u32;
        let mut aborted = Vec::new();
        let reqs = pool.take_requests(
            64,
            true,
            |_| {
                let i = next;
                next += 1;
                Some((i, plen))
            },
            |_| true,
            |i| aborted.push(i),
        );
        assert_eq!(pool.slots.len(), 2, "must not hog the freelist");
        assert_eq!(reqs.len(), 8); // 2 pieces × 4 blocks
        assert!(aborted.is_empty());
    }

    #[test]
    fn take_requests_releases_claim_when_start_fails() {
        let plen = BLOCK_SIZE;
        let mut pool = test_staging(1, plen);
        let mut aborted = Vec::new();
        let reqs = pool.take_requests(
            8,
            true,
            |_| Some((7, plen + 1)), // longer than piece_length → try_start fails
            |_| true,
            |i| aborted.push(i),
        );
        assert!(reqs.is_empty());
        assert_eq!(aborted, vec![7]);
        assert!(pool.is_empty());
    }
}
