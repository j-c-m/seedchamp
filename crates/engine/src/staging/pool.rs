//! Piece staging: assemble blocks in RAM, SHA-1 verify, then disk write.
//!
//! Per peer: assembling/hashing metadata; piece **bytes** from a shared
//! [`PieceBufferPool`] freelist (torrent `staging_mem_limit`).

use sha1::{Digest, Sha1};

use crate::disk::{write_piece, FdCache, StorageLayout};
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

/// Assembling piece temporarily removed from the pool for a Compio-owned fill.
pub struct TakenPieceBuf {
    piece: ActivePiece,
    pub buf: Vec<u8>,
}

impl TakenPieceBuf {
    pub fn index(&self) -> u32 {
        self.piece.index
    }

    pub fn piece_length(&self) -> u32 {
        self.piece.length
    }

    /// True if `[begin, begin+len)` fits the piece and buffer.
    pub fn range_ok(&self, begin: u32, len: u32) -> bool {
        if len == 0 {
            return false;
        }
        let end = begin as u64 + len as u64;
        end <= self.piece.length as u64 && (begin as usize + len as usize) <= self.buf.len()
    }
}

/// Standalone piece buffer for unit tests / sync commit path (not the peer freelist).
#[derive(Debug)]
pub struct PendingPiece {
    pub index: u32,
    pub length: u32,
    buf: Vec<u8>,
    have: Vec<bool>,
    requested: Vec<bool>,
    endgame: bool,
}

impl PendingPiece {
    pub fn new(index: u32, length: u32) -> Self {
        let nblocks = num_blocks(length);
        Self {
            index,
            length,
            buf: vec![0u8; length as usize],
            have: vec![false; nblocks],
            requested: vec![false; nblocks],
            endgame: false,
        }
    }

    pub fn num_blocks(&self) -> usize {
        self.have.len()
    }

    pub fn is_complete(&self) -> bool {
        self.have.iter().all(|&h| h)
    }

    pub fn set_endgame(&mut self, on: bool) {
        self.endgame = on;
    }

    pub fn requeue_missing(&mut self) {
        for (i, h) in self.have.iter().enumerate() {
            if !*h {
                self.requested[i] = false;
            }
        }
    }

    pub fn clear_request(&mut self, begin: u32, length: u32) -> bool {
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

    pub fn next_request(&mut self) -> Option<(u32, u32)> {
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

    pub fn outstanding_requests(&self) -> usize {
        self.have
            .iter()
            .zip(self.requested.iter())
            .filter(|(&h, &r)| !h && r)
            .count()
    }

    pub fn ingest(&mut self, begin: u32, data: &[u8]) -> Result<bool> {
        let mut active = ActivePiece {
            index: self.index,
            length: self.length,
            have: std::mem::take(&mut self.have),
            requested: std::mem::take(&mut self.requested),
            endgame: self.endgame,
        };
        let r = active.ingest(&mut self.buf, begin, data);
        self.have = active.have;
        self.requested = active.requested;
        self.endgame = active.endgame;
        r
    }

    pub fn data(&self) -> &[u8] {
        &self.buf
    }

    pub fn into_buf(self) -> Vec<u8> {
        self.buf
    }

    pub fn verify_sha1(&self, expected20: &[u8]) -> bool {
        if expected20.len() != 20 {
            return false;
        }
        let mut h = Sha1::new();
        h.update(&self.buf[..self.length as usize]);
        h.finalize().as_slice() == expected20
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

    pub fn has_pool(&self) -> bool {
        self.pool.is_some()
    }

    pub fn pool_arc(&self) -> Option<&std::sync::Arc<super::piece_pool::PieceBufferPool>> {
        self.pool.as_ref()
    }

    pub fn attach(&mut self, pool: std::sync::Arc<super::piece_pool::PieceBufferPool>) {
        self.pool = Some(pool);
    }

    /// Detach the shared freelist. Assembling buffers are dropped (not parked).
    /// Hashing slots stay until [`Self::reclaim`].
    pub fn abandon(&mut self) {
        let mut kept = Vec::new();
        for s in std::mem::take(&mut self.slots) {
            match s {
                Slot::Assembling { .. } => {
                    if let Some(ref pool) = self.pool {
                        pool.discard();
                    }
                }
                Slot::Hashing { index } => {
                    if let Some(ref pool) = self.pool {
                        pool.discard();
                    }
                    kept.push(Slot::Hashing { index });
                }
            }
        }
        self.slots = kept;
        self.pool = None;
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

    /// Take assembling piece buffer for Compio IoBuf fill (owned for the op).
    ///
    /// Caller must [`Self::put_assembling`] (or drop via freelist on fatal error).
    pub fn take_assembling(&mut self, index: u32) -> Option<TakenPieceBuf> {
        let pos = self
            .slots
            .iter()
            .position(|s| matches!(s, Slot::Assembling { piece, .. } if piece.index == index))?;
        let Slot::Assembling { piece, buf } = self.slots.swap_remove(pos) else {
            return None;
        };
        Some(TakenPieceBuf { piece, buf })
    }

    /// Restore a buffer taken with [`Self::take_assembling`].
    pub fn put_assembling(&mut self, taken: TakenPieceBuf) {
        self.slots.push(Slot::Assembling {
            piece: taken.piece,
            buf: taken.buf,
        });
    }

    /// After body written in place (Compio fill or copy), mark have-bits.
    /// `Ok(None)` if not assembling; `Ok(Some(piece_complete))` otherwise.
    pub fn finish_block_range(
        &mut self,
        index: u32,
        begin: u32,
        length: u32,
    ) -> Result<Option<bool>> {
        let Some((piece, buf)) = self.find_assembling_mut(index) else {
            return Ok(None);
        };
        piece.mark_range_have(begin, length, buf.len() as u32)?;
        Ok(Some(piece.is_complete()))
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
        self.slots.push(Slot::Hashing { index });
        Some((piece.length, buf))
    }

    /// Return a buffer after hash/disk to the shared freelist.
    pub fn reclaim(&mut self, index: u32, data: Vec<u8>) {
        self.slots
            .retain(|s| !matches!(s, Slot::Hashing { index: i } if *i == index));
        if let Some(ref pool) = self.pool {
            pool.release(data);
        }
        // else drop data (no pool)
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
    pub fn take_requests(
        &mut self,
        pipeline: usize,
        endgame: bool,
        mut start_piece: impl FnMut(&Self) -> Option<(u32, u32)>,
        mut may_request_piece: impl FnMut(u32) -> bool,
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
            if !self.can_start_more() {
                break;
            }
            let Some((index, plen)) = start_piece(self) else {
                break;
            };
            if self.contains(index) {
                break;
            }
            if !self.try_start(index, plen) {
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

/// Verify staged piece SHA-1 then write to disk. Returns Ok if written.
pub fn commit_verified_piece(
    cache: &mut FdCache,
    layout: &StorageLayout,
    piece: &PendingPiece,
    expected_hash: &[u8],
) -> Result<()> {
    if !piece.is_complete() {
        return Err(Error::Msg("piece not complete".into()));
    }
    if !piece.verify_sha1(expected_hash) {
        return Err(Error::Msg(format!(
            "piece {} hash mismatch (corrupt)",
            piece.index
        )));
    }
    write_piece(cache, layout, piece.index, piece.data())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk::spans::FileLayout;
    use crate::disk::{ensure_storage, read_piece};
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

        let mut p = PendingPiece::new(0, len as u32);
        assert_eq!(p.num_blocks(), 3);
        assert!(!p.ingest(0, &data[0..BLOCK_SIZE as usize]).unwrap());
        assert!(!p
            .ingest(
                BLOCK_SIZE,
                &data[BLOCK_SIZE as usize..2 * BLOCK_SIZE as usize]
            )
            .unwrap());
        assert!(p
            .ingest(2 * BLOCK_SIZE, &data[2 * BLOCK_SIZE as usize..])
            .unwrap());
        assert!(p.verify_sha1(&digest));

        let mut cache = FdCache::default_cache();
        commit_verified_piece(&mut cache, &layout, &p, &digest).unwrap();
        let mut out = Vec::new();
        read_piece(&mut cache, &layout, 0, &mut out).unwrap();
        assert_eq!(out, data);
    }

    #[test]
    fn reject_clears_only_one_block() {
        let len = BLOCK_SIZE * 3;
        let mut p = PendingPiece::new(0, len);
        let _ = p.next_request().unwrap();
        let _ = p.next_request().unwrap();
        let _ = p.next_request().unwrap();
        assert_eq!(p.outstanding_requests(), 3);
        assert!(p.clear_request(BLOCK_SIZE, BLOCK_SIZE));
        assert_eq!(p.outstanding_requests(), 2);
        let again = p.next_request().unwrap();
        assert_eq!(again, (BLOCK_SIZE, BLOCK_SIZE));
        assert!(p.next_request().is_none());
        assert!(!p.clear_request(0, 1));
        assert_eq!(p.outstanding_requests(), 3);
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
        assert!(!pool.can_start_more());
        pool.reclaim(0, buf);
        assert!(pool.can_start_more());
        assert!(pool.try_start(2, plen));
    }

    #[test]
    fn pipeline_requests() {
        let len = BLOCK_SIZE * 2 + 100;
        let mut p = PendingPiece::new(0, len);
        let r1 = p.next_request().unwrap();
        assert_eq!(r1, (0, BLOCK_SIZE));
        let r2 = p.next_request().unwrap();
        assert_eq!(r2, (BLOCK_SIZE, BLOCK_SIZE));
        let r3 = p.next_request().unwrap();
        assert_eq!(r3, (2 * BLOCK_SIZE, 100));
        assert!(p.next_request().is_none());
        p.set_endgame(true);
        assert!(p.next_request().is_none());
        p.requeue_missing();
        let again = p.next_request().unwrap();
        assert_eq!(again, (0, BLOCK_SIZE));
        let next = p.next_request().unwrap();
        assert_eq!(next.0, BLOCK_SIZE);
        assert!(p.next_request().is_some());
        assert!(p.next_request().is_none());
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
        );
        assert_eq!(reqs.len(), 32, "should queue full pipeline in one shot");
        assert_eq!(pool.total_outstanding(), 32);
        let mut seen = std::collections::HashSet::new();
        for (i, b, l) in &reqs {
            assert!(seen.insert((*i, *b)), "duplicate request {i}:{b}");
            assert_eq!(*l, BLOCK_SIZE);
        }
        let more = pool.take_requests(32, false, |_| Some((99, plen)), |_| true);
        assert!(more.is_empty());
    }

    #[test]
    fn take_requests_respects_may_request_piece() {
        let plen = BLOCK_SIZE * 4;
        let mut pool = test_staging(4, plen);
        assert!(pool.try_start(0, plen));
        assert!(pool.try_start(1, plen));
        let reqs = pool.take_requests(8, false, |_| None, |i| i == 1);
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
    fn abandon_drops_assembling_without_parking() {
        let shared = Arc::new(super::super::piece_pool::PieceBufferPool::new(
            BLOCK_SIZE,
            2 * BLOCK_SIZE as u64,
        ));
        let mut pool = StagingPool::from_pool(Arc::clone(&shared));
        assert!(pool.try_start(0, BLOCK_SIZE));
        assert_eq!(shared.freelist_len(), 0);
        assert_eq!(shared.available(), 1);
        pool.abandon();
        assert!(!pool.has_pool());
        assert_eq!(shared.freelist_len(), 0);
        assert_eq!(shared.available(), 2);
    }

    #[test]
    fn reclaim_after_abandon_does_not_park() {
        let shared = Arc::new(super::super::piece_pool::PieceBufferPool::new(
            BLOCK_SIZE,
            2 * BLOCK_SIZE as u64,
        ));
        let mut pool = StagingPool::from_pool(Arc::clone(&shared));
        assert!(pool.try_start(0, BLOCK_SIZE));
        let data = vec![1u8; BLOCK_SIZE as usize];
        assert_eq!(pool.ingest_if_staged(0, 0, &data).unwrap(), Some(true));
        let (_len, buf) = pool.take_for_hash(0).unwrap();
        pool.abandon();
        assert_eq!(shared.freelist_len(), 0);
        pool.reclaim(0, buf);
        assert_eq!(shared.freelist_len(), 0);
        assert_eq!(shared.available(), 2);
    }

    #[test]
    fn attach_after_abandon_uses_new_pool() {
        let old = Arc::new(super::super::piece_pool::PieceBufferPool::new(
            BLOCK_SIZE,
            2 * BLOCK_SIZE as u64,
        ));
        let new = Arc::new(super::super::piece_pool::PieceBufferPool::new(
            BLOCK_SIZE,
            2 * BLOCK_SIZE as u64,
        ));
        let mut pool = StagingPool::from_pool(Arc::clone(&old));
        assert!(pool.try_start(0, BLOCK_SIZE));
        pool.abandon();
        assert!(!pool.has_pool());
        pool.attach(Arc::clone(&new));
        assert!(std::sync::Arc::ptr_eq(pool.pool_arc().unwrap(), &new));
        assert!(pool.try_start(1, BLOCK_SIZE));
        assert_eq!(new.available(), 1);
        assert_eq!(old.freelist_len(), 0);
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
        // Hash holds one buffer; one remains on freelist.
        assert_eq!(pool.free_slots(), 1);
        pool.reclaim(0, buf);
        assert_eq!(pool.free_slots(), 2);
        assert!(pool.try_start(1, plen));
    }

    /// Take/put Compio path + finish_block_range must match ingest.
    #[test]
    fn take_put_finish_match_ingest() {
        let plen = BLOCK_SIZE * 2;
        let mut pool = test_staging(2, plen);
        let block0: Vec<u8> = (0..BLOCK_SIZE as usize).map(|i| (i % 251) as u8).collect();
        let block1: Vec<u8> = (0..BLOCK_SIZE as usize)
            .map(|i| ((i * 3) % 251) as u8)
            .collect();

        assert!(pool.take_assembling(0).is_none());
        assert!(pool.finish_block_range(0, 0, BLOCK_SIZE).unwrap().is_none());

        assert!(pool.try_start(0, plen));
        {
            let mut t = pool.take_assembling(0).expect("taken");
            assert!(t.range_ok(0, BLOCK_SIZE));
            assert!(!t.range_ok(0, 0));
            assert!(!t.range_ok(0, plen + 1));
            t.buf[..BLOCK_SIZE as usize].copy_from_slice(&block0);
            pool.put_assembling(t);
        }
        assert_eq!(
            pool.finish_block_range(0, 0, BLOCK_SIZE).unwrap(),
            Some(false)
        );

        {
            let mut t = pool.take_assembling(0).expect("taken");
            t.buf[BLOCK_SIZE as usize..].copy_from_slice(&block1);
            pool.put_assembling(t);
        }
        assert_eq!(
            pool.finish_block_range(0, BLOCK_SIZE, BLOCK_SIZE).unwrap(),
            Some(true)
        );

        let (got_len, buf) = pool.take_for_hash(0).unwrap();
        assert_eq!(got_len, plen);
        assert_eq!(&buf[..BLOCK_SIZE as usize], block0.as_slice());
        assert_eq!(&buf[BLOCK_SIZE as usize..], block1.as_slice());
        pool.reclaim(0, buf);
    }

    #[test]
    fn take_put_finish_equivalent_to_ingest_bytes() {
        let plen = BLOCK_SIZE * 2;
        let b0 = vec![0x11u8; BLOCK_SIZE as usize];
        let b1 = vec![0x22u8; BLOCK_SIZE as usize];

        let mut direct = test_staging(2, plen);
        assert!(direct.try_start(0, plen));
        {
            let mut t = direct.take_assembling(0).unwrap();
            t.buf[..BLOCK_SIZE as usize].copy_from_slice(&b0);
            direct.put_assembling(t);
        }
        direct.finish_block_range(0, 0, BLOCK_SIZE).unwrap();
        {
            let mut t = direct.take_assembling(0).unwrap();
            t.buf[BLOCK_SIZE as usize..].copy_from_slice(&b1);
            direct.put_assembling(t);
        }
        assert_eq!(
            direct
                .finish_block_range(0, BLOCK_SIZE, BLOCK_SIZE)
                .unwrap(),
            Some(true)
        );
        let (_, direct_buf) = direct.take_for_hash(0).unwrap();

        let mut via_ingest = test_staging(2, plen);
        assert!(via_ingest.try_start(0, plen));
        via_ingest.ingest_if_staged(0, 0, &b0).unwrap();
        assert_eq!(
            via_ingest.ingest_if_staged(0, BLOCK_SIZE, &b1).unwrap(),
            Some(true)
        );
        let (_, ingest_buf) = via_ingest.take_for_hash(0).unwrap();

        assert_eq!(direct_buf, ingest_buf);
    }
}
