//! Per-peer download state (we leech FROM them) and piece ingest / hash outcomes.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

use flume::Sender as FlumeSender;

use crate::error::Result;
use crate::staging::StagingPool;
use crate::wire::PieceHeader;

use super::config::PeerConfig;
use super::ctrl_scratch::CtrlScratch;
use super::duplex::PeerOut;
use super::helpers::{can_request_from, publish_dl_queue, push_suggest, take_suggested};
use crate::catalog::bitfield_get;
use crate::hot::{HotTorrent, PeerAvailability};
use crate::runtime::{HashJob, HashOutcome};

/// Per-session piece claims (exclusive outside endgame; released on Drop / mark_have).
pub(crate) struct PeerPieceClaims {
    torrent: Arc<HotTorrent>,
    held: std::sync::Mutex<HashSet<u32>>,
}

impl PeerPieceClaims {
    pub(crate) fn new(torrent: Arc<HotTorrent>) -> Self {
        Self {
            torrent,
            held: std::sync::Mutex::new(HashSet::new()),
        }
    }

    pub(crate) fn try_claim(&self, index: u32, endgame: bool) -> bool {
        if !self.torrent.try_claim_piece(index, endgame) {
            return false;
        }
        if let Ok(mut h) = self.held.lock() {
            h.insert(index);
        }
        true
    }
}

impl Drop for PeerPieceClaims {
    fn drop(&mut self) {
        if let Ok(mut h) = self.held.lock() {
            for i in h.drain() {
                if !self.torrent.has_piece(i) {
                    self.torrent.release_piece_claim(i);
                }
            }
        }
    }
}

/// Per-peer download half: staging, pick state, choke/AF, remote bitfield.
///
/// Outbound control/PIECE upload stays on [`OutQueue`]; this only decides and
/// issues **our** Requests and tracks blocks/pieces coming from the remote.
pub(crate) struct PeerDownload {
    pub torrent: Arc<HotTorrent>,
    pub peer_bf: Vec<u8>,
    pub staging: StagingPool,
    pub pipeline: usize,
    pub hashing: HashSet<u32>,
    pub endgame: bool,
    pub claims: PeerPieceClaims,
    pub peer_choking: bool,
    /// Allowed Fast from remote (may Request while choked).
    pub allowed_fast: HashSet<u32>,
    /// Suggest Piece from remote (newest first). Advisory; does not bypass choke.
    pub suggested: VecDeque<u32>,
    pub peer_avail: Option<PeerAvailability>,
    /// Reused encode buffer for Request / Cancel batches.
    ctrl: CtrlScratch,
}

impl PeerDownload {
    pub(crate) fn new(
        torrent: Arc<HotTorrent>,
        peer_bf: Vec<u8>,
        staging: StagingPool,
        pipeline: usize,
        peer_avail: Option<PeerAvailability>,
    ) -> Self {
        let claims = PeerPieceClaims::new(Arc::clone(&torrent));
        Self {
            torrent,
            peer_bf,
            staging,
            pipeline,
            hashing: HashSet::new(),
            endgame: false,
            claims,
            peer_choking: true,
            allowed_fast: HashSet::new(),
            suggested: VecDeque::new(),
            peer_avail,
            ctrl: CtrlScratch::new(),
        }
    }

    #[inline]
    pub(crate) fn can_request(&self) -> bool {
        can_request_from(self.peer_choking, &self.allowed_fast)
    }

    /// Store a Suggest Piece index from this peer (cap 32, newest first).
    pub(crate) fn push_suggest(&mut self, index: u32) {
        push_suggest(&mut self.suggested, index, self.torrent.piece_count);
    }

    #[inline]
    pub(crate) fn outstanding(&self) -> u64 {
        self.staging.total_outstanding() as u64
    }

    /// Expect a 13-byte PIECE header: leeching with outstanding Requests.
    #[inline]
    pub(crate) fn expect_piece_header(&self, downloading: bool) -> bool {
        downloading && self.outstanding() > 0
    }

    /// Whether this PIECE header should fill staging (else discard body).
    pub(crate) fn want_direct_piece(&self, h: &PieceHeader) -> bool {
        h.block_len > 0
            && !self.torrent.has_piece(h.index)
            && !self.hashing.contains(&h.index)
            && self.staging.contains(h.index)
    }

    /// After Compio body fill into staging; returns whether pipeline should refill.
    pub(crate) fn finish_direct_piece_body(
        &mut self,
        hash_tx: &FlumeSender<HashOutcome>,
        hash_pool: &crate::runtime::HashPool,
        index: u32,
        begin: u32,
        total: u32,
    ) -> Result<bool> {
        // Another peer may have completed this piece while we were mid-body.
        if self.torrent.has_piece(index) || self.hashing.contains(&index) {
            return Ok(true);
        }
        let Some(complete) = self.staging.finish_block_range(index, begin, total)? else {
            return Ok(false);
        };
        if !complete {
            return Ok(true);
        }
        let Some((plen, data)) = self.staging.take_for_hash(index) else {
            return Ok(true);
        };
        let expected = match self.torrent.piece_hash(index) {
            Ok(h) => h,
            Err(e) => {
                self.staging.reclaim(index, data);
                return Err(e);
            }
        };
        let layout = self.torrent.layout();
        self.hashing.insert(index);
        match hash_pool.submit(HashJob {
            index,
            plen,
            data,
            expected,
            layout,
            reply: hash_tx.clone(),
        }) {
            Ok(()) => Ok(true),
            Err((e, job)) => {
                self.hashing.remove(&index);
                self.staging.reclaim(index, job.data);
                Err(e)
            }
        }
    }

    /// Cancel all currently outstanding block requests into the outbound queue.
    pub(crate) fn cancel_outstanding(&mut self, out: &mut impl PeerOut) -> bool {
        let list = self.staging.outstanding_list();
        if list.is_empty() {
            return false;
        }
        self.ctrl.clear();
        self.ctrl.reserve_requests(list.len());
        for (index, begin, length) in list {
            self.ctrl.append_cancel(index, begin, length);
        }
        out.push_ctrl_owned(self.ctrl.take());
        true
    }

    /// Pick and enqueue Request messages (non-blocking). Returns true if any enqueued.
    pub(crate) fn issue_requests(&mut self, out: &mut impl PeerOut, cfg: &PeerConfig) -> bool {
        // Global download cap: reserve budget before take_requests (avoids stranded outstanding).
        const BLOCK: u64 = crate::staging::BLOCK_SIZE as u64;
        let max_blocks = if let Some(lim) = cfg.wire_limiter.as_ref() {
            if lim.download_unlimited() {
                self.pipeline
            } else {
                let pre = lim.try_consume_download(self.pipeline as u64 * BLOCK);
                if pre < BLOCK {
                    // try_consume deducted a partial grant — put it back.
                    if pre > 0 {
                        lim.refund_download(pre);
                    }
                    return false;
                }
                (pre / BLOCK) as usize
            }
        } else {
            self.pipeline
        };
        let peer_choking = self.peer_choking;
        let allowed = &self.allowed_fast;
        let may = |i: u32| !peer_choking || allowed.contains(&i);
        let hashing = &self.hashing;
        let claims = &self.claims;
        let endgame = self.endgame;
        let torrent = &self.torrent;
        let peer_bf = self.peer_bf.as_slice();
        let suggested = &mut self.suggested;
        let reqs = self.staging.take_requests(
            max_blocks,
            endgame,
            |st| {
                start_piece(
                    torrent,
                    peer_bf,
                    suggested,
                    |i| st.contains(i),
                    |i| hashing.contains(&i),
                    |i| claims.try_claim(i, endgame),
                    may,
                    endgame,
                )
            },
            may,
        );
        if reqs.is_empty() {
            // Refund full pre-consume when limited.
            if let Some(lim) = cfg.wire_limiter.as_ref() {
                if !lim.download_unlimited() {
                    lim.refund_download(max_blocks as u64 * BLOCK);
                }
            }
            return false;
        }
        // Pre-consumed max_blocks * BLOCK; actual wire may be smaller (tail blocks).
        if let Some(lim) = cfg.wire_limiter.as_ref() {
            if !lim.download_unlimited() {
                let actual: u64 = reqs.iter().map(|(_, _, l)| *l as u64).sum();
                let reserved = max_blocks as u64 * BLOCK;
                if reserved > actual {
                    lim.refund_download(reserved - actual);
                }
            }
        }
        self.ctrl.clear();
        self.ctrl.reserve_requests(reqs.len());
        for (index, begin, length) in reqs {
            self.ctrl.append_request(index, begin, length);
        }
        out.push_ctrl_owned(self.ctrl.take());
        true
    }

    /// Maybe refill Requests and publish TUI download queue counters.
    pub(crate) fn refresh_outbound(
        &mut self,
        out: &mut impl PeerOut,
        cfg: &PeerConfig,
        downloading: bool,
        need_fill: bool,
    ) {
        if need_fill && downloading && self.can_request() {
            let _ = self.issue_requests(out, cfg);
        }
        if downloading {
            publish_dl_queue(cfg, self.outstanding(), self.pipeline as u64);
        }
    }

    /// Ingest a wire block; submit complete pieces to the hash pool.
    ///
    /// Returns `Ok(true)` if the block was written into staging (caller may
    /// count `wire_down`). `Ok(false)` for no-ops: already have / hashing /
    /// not staged.
    pub(crate) fn handle_piece(
        &mut self,
        hash_tx: &FlumeSender<HashOutcome>,
        hash_pool: &crate::runtime::HashPool,
        index: u32,
        begin: u32,
        block: &[u8],
    ) -> Result<bool> {
        if self.torrent.has_piece(index) || self.hashing.contains(&index) {
            return Ok(false);
        }
        let Some(complete) = self.staging.ingest_if_staged(index, begin, block)? else {
            return Ok(false);
        };
        if !complete {
            return Ok(true);
        }
        let Some((plen, data)) = self.staging.take_for_hash(index) else {
            return Ok(true);
        };
        let expected = match self.torrent.piece_hash(index) {
            Ok(h) => h,
            Err(e) => {
                self.staging.reclaim(index, data);
                return Err(e);
            }
        };
        let layout = self.torrent.layout();
        self.hashing.insert(index);

        match hash_pool.submit(HashJob {
            index,
            plen,
            data,
            expected,
            layout,
            reply: hash_tx.clone(),
        }) {
            Ok(()) => Ok(true),
            Err((e, job)) => {
                self.hashing.remove(&index);
                self.staging.reclaim(index, job.data);
                Err(e)
            }
        }
    }

    /// Apply one hash job result. Returns true if the request pipeline should refill.
    pub(crate) fn apply_hash_outcome(
        &mut self,
        outcome: HashOutcome,
        on_piece: Option<&Arc<dyn Fn(i64, u32, u32) + Send + Sync>>,
    ) -> bool {
        match outcome {
            HashOutcome::Ok { index, plen, data } => {
                self.hashing.remove(&index);
                self.staging.reclaim(index, data);
                if !self.torrent.has_piece(index) {
                    self.torrent.mark_have(index);
                    if let Some(cb) = on_piece {
                        cb(self.torrent.id, index, plen);
                    }
                }
                true
            }
            HashOutcome::HashFail { index, plen, data } => {
                self.hashing.remove(&index);
                self.staging.reclaim(index, data);
                self.torrent.release_piece_claim(index);
                tracing::warn!(piece = index, plen, "piece hash failed — re-pick later");
                true
            }
            HashOutcome::WriteFail { index, plen, data } => {
                self.hashing.remove(&index);
                self.staging.reclaim(index, data);
                self.torrent.release_piece_claim(index);
                tracing::warn!(
                    piece = index,
                    plen,
                    "piece disk write failed — re-pick later"
                );
                true
            }
        }
    }
}

/// First eligible Suggest Piece, else rarest-first.
fn start_piece(
    torrent: &HotTorrent,
    peer_bf: &[u8],
    suggested: &mut VecDeque<u32>,
    in_staging: impl Fn(u32) -> bool,
    is_hashing: impl Fn(u32) -> bool,
    mut try_claim: impl FnMut(u32) -> bool,
    piece_ok: impl Fn(u32) -> bool,
    endgame: bool,
) -> Option<(u32, u32)> {
    if let Some(i) = take_suggested(suggested, |i| {
        torrent.wants_piece(i)
            && !torrent.has_piece(i)
            && bitfield_get(peer_bf, i)
            && !in_staging(i)
            && !is_hashing(i)
            && piece_ok(i)
            && try_claim(i)
    }) {
        return torrent.layout().piece_size(i).ok().map(|plen| (i, plen));
    }
    torrent.pick_rarest_piece(
        peer_bf, in_staging, is_hashing, try_claim, piece_ok, endgame,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{bitfield_set, empty_bitfield};
    use crate::disk::{FileLayout, StorageLayout};
    use std::path::PathBuf;

    fn two_wanted_layout() -> StorageLayout {
        StorageLayout {
            data_root: PathBuf::from("/tmp"),
            piece_length: 32,
            piece_count: 2,
            total_size: 64,
            files: vec![
                FileLayout {
                    path: PathBuf::from("a"),
                    size: 32,
                    offset: 0,
                    priority: 1,
                },
                FileLayout {
                    path: PathBuf::from("b"),
                    size: 32,
                    offset: 32,
                    priority: 1,
                },
            ],
        }
    }

    fn torrent() -> Arc<HotTorrent> {
        Arc::new(HotTorrent::new_empty(
            1,
            [0u8; 20],
            "t".into(),
            two_wanted_layout(),
            vec![0u8; 40],
        ))
    }

    fn all_peer_bf() -> Vec<u8> {
        let mut bf = empty_bitfield(2);
        bitfield_set(&mut bf, 0);
        bitfield_set(&mut bf, 1);
        bf
    }

    #[test]
    fn suggested_beats_rarer_piece() {
        let t = torrent();
        t.avail_inc(0);
        t.avail_inc(0);
        t.avail_inc(0);
        t.avail_inc(1); // rarer
        let peer_bf = all_peer_bf();
        let mut suggested = VecDeque::from([0]);
        let mut claimed = HashSet::new();
        let pick = start_piece(
            &t,
            &peer_bf,
            &mut suggested,
            |_| false,
            |_| false,
            |i| claimed.insert(i),
            |_| true,
            false,
        );
        assert_eq!(pick.map(|p| p.0), Some(0));
    }

    #[test]
    fn ineligible_suggest_falls_back_to_rarest() {
        let t = torrent();
        t.avail_inc(0);
        t.avail_inc(0);
        t.avail_inc(0);
        t.avail_inc(1); // rarer
        let mut peer_bf = empty_bitfield(2);
        bitfield_set(&mut peer_bf, 1); // peer lacks suggested 0
        let mut suggested = VecDeque::from([0]);
        let mut claimed = HashSet::new();
        let pick = start_piece(
            &t,
            &peer_bf,
            &mut suggested,
            |_| false,
            |_| false,
            |i| claimed.insert(i),
            |_| true,
            false,
        );
        assert_eq!(pick.map(|p| p.0), Some(1));
        assert!(suggested.is_empty());
    }

    #[test]
    fn choked_suggest_without_allowed_fast_is_not_picked() {
        let t = torrent();
        t.avail_inc(0);
        t.avail_inc(1);
        let peer_bf = all_peer_bf();
        let mut suggested = VecDeque::from([0]);
        let mut claimed = HashSet::new();
        let pick = start_piece(
            &t,
            &peer_bf,
            &mut suggested,
            |_| false,
            |_| false,
            |i| claimed.insert(i),
            |_| false,
            false,
        );
        assert!(pick.is_none());
    }

    #[test]
    fn choked_suggest_in_allowed_fast_is_picked() {
        let t = torrent();
        t.avail_inc(0);
        t.avail_inc(0);
        t.avail_inc(0);
        t.avail_inc(1); // rarer, but not Allowed Fast
        let peer_bf = all_peer_bf();
        let mut suggested = VecDeque::from([0]);
        let mut claimed = HashSet::new();
        let pick = start_piece(
            &t,
            &peer_bf,
            &mut suggested,
            |_| false,
            |_| false,
            |i| claimed.insert(i),
            |i| i == 0,
            false,
        );
        assert_eq!(pick.map(|p| p.0), Some(0));
    }

    #[test]
    fn suggest_already_have_or_unwanted_is_dropped() {
        let t = torrent();
        t.mark_have(0);
        let peer_bf = all_peer_bf();
        let mut suggested = VecDeque::from([0]);
        let mut claimed = HashSet::new();
        let pick = start_piece(
            &t,
            &peer_bf,
            &mut suggested,
            |_| false,
            |_| false,
            |i| claimed.insert(i),
            |_| true,
            false,
        );
        assert_eq!(pick.map(|p| p.0), Some(1));
        assert!(suggested.is_empty());
    }
}
