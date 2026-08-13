//! Ordered outbound queue: control messages + PIECE payloads.
//!
//! Wire order is explicit. After finishing the **active** message:
//! pending **control** before the next **PIECE** (never preempt mid-frame).
//!
//! RC4 is applied only when an item becomes **wire-active**, so cipher order
//! matches send order. Piece payloads are filled in [`begin_upload`] (Compio
//! `read_at`, or blocking `pread` when configured) before wire-active write.

use std::collections::VecDeque;
use std::sync::atomic::Ordering;

use compio::net::TcpStream;

use super::config::PeerConfig;
use super::ctrl_scratch::CtrlScratch;
use super::helpers::publish_upload_pending;
use crate::crypto::Rc4;
use crate::error::{Error, Result};
use crate::hot::HotTorrent;
use crate::upload::{
    begin_upload, InFlightUpload, UploadBlock, MAX_REQUEST_LENGTH, MAX_UPLOAD_REQQ,
};

enum Active {
    /// Ciphertext (or plain) for Compio owned `write_all`.
    Ctrl { buf: Vec<u8> },
    /// PIECE mid-wire (Buffered in scratch).
    Piece(InFlightUpload),
}

/// Peer outbound: ctrl queue + piece queue + single active wire send.
pub struct OutQueue {
    ctrl: VecDeque<Vec<u8>>,
    pieces: VecDeque<UploadBlock>,
    active: Option<Active>,
    piece_max: usize,
    /// At most one KeepAlive may sit in ctrl/active until fully written.
    keepalive_queued: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutProgress {
    /// Nothing left to send.
    Idle,
    /// Wrote ≥1 byte this quantum (may still have more work).
    Progress,
    /// Global upload rate limit: no tokens for the next PIECE (control still free).
    /// Sleep this long before retrying (token refill estimate).
    RateLimited(std::time::Duration),
}

/// Result of [`OutQueue::send_quantum`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SendOutcome {
    pub kind: OutProgress,
    /// True if any byte was written to the socket this call.
    pub wrote: bool,
}

impl SendOutcome {
    fn idle() -> Self {
        Self {
            kind: OutProgress::Idle,
            wrote: false,
        }
    }
}

impl OutQueue {
    pub fn new() -> Self {
        Self {
            ctrl: VecDeque::new(),
            pieces: VecDeque::new(),
            active: None,
            piece_max: MAX_UPLOAD_REQQ,
            keepalive_queued: false,
        }
    }

    pub fn has_work(&self) -> bool {
        self.active.is_some() || !self.ctrl.is_empty() || !self.pieces.is_empty()
    }

    /// True if `send_quantum` can make socket progress.
    pub fn can_progress_write(&self) -> bool {
        self.active.is_some() || !self.ctrl.is_empty() || !self.pieces.is_empty()
    }

    pub fn piece_pending_count(&self) -> usize {
        let active = matches!(self.active, Some(Active::Piece(_))) as usize;
        active + self.pieces.len()
    }

    /// Enqueue owned control frame bytes (HAVE, Request, Cancel, …).
    /// Coalesces into the last ctrl chunk when possible.
    pub fn push_ctrl_owned(&mut self, plain: Vec<u8>) {
        if plain.is_empty() {
            return;
        }
        const COALESCE_MAX: usize = 64 * 1024;
        if let Some(back) = self.ctrl.back_mut() {
            if back.len() + plain.len() <= COALESCE_MAX {
                back.extend_from_slice(&plain);
                return;
            }
        }
        self.ctrl.push_back(plain);
    }

    /// Enqueue a single BEP3 KeepAlive if one is not already queued/in-flight.
    pub fn push_keepalive(&mut self, ctrl: &mut CtrlScratch) {
        if self.keepalive_queued {
            return;
        }
        self.keepalive_queued = true;
        ctrl.clear();
        ctrl.append_keepalive();
        self.push_ctrl_owned(ctrl.take());
    }

    fn note_ctrl_fully_sent(&mut self) {
        if self.ctrl.is_empty() && !matches!(self.active, Some(Active::Ctrl { .. })) {
            self.keepalive_queued = false;
        }
    }

    /// Queue a PIECE to serve (upload Request). Dedupes active and FIFO.
    pub fn try_push_piece(&mut self, block: UploadBlock) -> bool {
        if block.length == 0 || block.length > MAX_REQUEST_LENGTH {
            return false;
        }
        if self.piece_pending_count() >= self.piece_max {
            return false;
        }
        if self.pieces.iter().any(|b| *b == block) {
            return false;
        }
        if let Some(Active::Piece(inf)) = &self.active {
            if inf.block == block {
                return false;
            }
        }
        self.pieces.push_back(block);
        true
    }

    /// Cancel a queued upload request.
    ///
    /// Removes from the piece FIFO. Drops **active** PIECE only when
    /// [`InFlightUpload::can_abort`]. If cipher/wire has started, the active
    /// PIECE is left to finish.
    pub fn cancel_piece(&mut self, block: UploadBlock) -> bool {
        let mut removed = false;
        if let Some(i) = self.pieces.iter().position(|b| *b == block) {
            self.pieces.remove(i);
            removed = true;
        }
        if let Some(Active::Piece(inf)) = &self.active {
            if inf.block == block {
                if inf.can_abort() {
                    self.active = None;
                    return true;
                }
                return removed;
            }
        }
        removed
    }

    /// Drop pending PIECE work (e.g. peer NotInterested).
    pub fn clear_pieces(&mut self) {
        self.pieces.clear();
        if let Some(Active::Piece(inf)) = &self.active {
            if inf.can_abort() {
                self.active = None;
            }
        }
    }

    /// True if an active PIECE is held that cannot be aborted (finishing after clear/cancel).
    #[cfg(test)]
    pub fn has_unabortable_piece(&self) -> bool {
        matches!(&self.active, Some(Active::Piece(inf)) if !inf.can_abort())
    }

    fn push_reject(&mut self, block: UploadBlock, fast_enabled: bool, ctrl: &mut CtrlScratch) {
        if !fast_enabled {
            return;
        }
        ctrl.clear();
        ctrl.append_reject_request(block.index, block.begin, block.length);
        self.push_ctrl_owned(ctrl.take());
    }

    /// Pick next wire-active: ctrl first, then fill+activate next piece.
    async fn pick_next_active(
        &mut self,
        torrent: &HotTorrent,
        cfg: &PeerConfig,
        rc4: &mut Option<&mut Rc4>,
        scratch: &mut Vec<u8>,
        encode: &mut CtrlScratch,
        fast_enabled: bool,
    ) -> Result<()> {
        if self.active.is_some() {
            return Ok(());
        }
        if let Some(mut plain) = self.ctrl.pop_front() {
            if let Some(c) = rc4.as_deref_mut() {
                c.crypt_inplace(&mut plain);
            }
            self.active = Some(Active::Ctrl { buf: plain });
            return Ok(());
        }

        if let Some(block) = self.pieces.pop_front() {
            let mut fill = begin_upload(
                &torrent.layout(),
                block,
                rc4.as_deref_mut(),
                cfg.upload,
                scratch,
            )
            .await;
            if matches!(&fill, Err(e) if e.is_not_found()) {
                fill = begin_upload(
                    &torrent.layout(),
                    block,
                    rc4.as_deref_mut(),
                    cfg.upload,
                    scratch,
                )
                .await;
            }
            match fill {
                Ok(inf) => {
                    self.active = Some(Active::Piece(inf));
                }
                Err(e) => {
                    tracing::debug!(
                        piece = block.index,
                        begin = block.begin,
                        length = block.length,
                        error = %e,
                        "upload begin failed — drop piece, keep peer"
                    );
                    self.push_reject(block, fast_enabled, encode);
                }
            }
        }
        Ok(())
    }

    /// Drive outbound work: Compio `write_all` each active frame fully.
    ///
    /// After each completed message, prefers control before the next PIECE.
    /// Piece fill is in [`Self::pick_next_active`] (Compio or blocking pread).
    pub async fn send_quantum(
        &mut self,
        stream: &mut TcpStream,
        torrent: &HotTorrent,
        cfg: &PeerConfig,
        mut rc4: Option<&mut Rc4>,
        scratch: &mut Vec<u8>,
        encode: &mut CtrlScratch,
        allow_upload: bool,
        fast_enabled: bool,
    ) -> Result<SendOutcome> {
        let mut any = false;

        if !allow_upload {
            self.pieces.clear();
        }

        loop {
            if self.active.is_none() {
                if !allow_upload && self.ctrl.is_empty() {
                    break;
                }
                self.pick_next_active(torrent, cfg, &mut rc4, scratch, encode, fast_enabled)
                    .await?;
                if self.active.is_none() {
                    if !allow_upload {
                        break;
                    }
                    if self.pieces.is_empty() && self.ctrl.is_empty() {
                        break;
                    }
                    // begin failed (no active): skip and try next piece
                    if self.ctrl.is_empty() && !self.pieces.is_empty() {
                        continue;
                    }
                    break;
                }
            }

            if !allow_upload {
                if let Some(Active::Piece(inf)) = &self.active {
                    if inf.can_abort() {
                        self.active = None;
                        continue;
                    }
                }
            }

            if matches!(self.active, Some(Active::Ctrl { .. })) {
                let Some(Active::Ctrl { buf }) = self.active.take() else {
                    unreachable!();
                };
                match crate::net::write_all_owned(stream, buf).await {
                    Ok(_buf) => {
                        any = true;
                        self.note_ctrl_fully_sent();
                        continue;
                    }
                    Err((e, _buf)) => {
                        // Drop partial ctrl on write error; session is toast either way.
                        publish_upload_pending(cfg, self.piece_pending_count());
                        return Err(e);
                    }
                }
            }

            let Some(Active::Piece(inf)) = self.active.as_mut() else {
                break;
            };
            let block = inf.block;
            if !inf.any_wire_bytes() {
                if let Some(lim) = cfg.wire_limiter.as_ref() {
                    let need = block.length as u64;
                    let got = lim.try_consume_upload(need);
                    if got < need {
                        if got > 0 {
                            lim.refund_upload(got);
                        }
                        // Precise wait for a full block — fixed 50ms undershoots
                        // (~90ms at smoke 2M/11.5 cap) and thrashing slows the cell.
                        let wait = lim.upload_delay_for(need);
                        let wait = if wait.is_zero() {
                            std::time::Duration::from_millis(1)
                        } else {
                            wait
                        };
                        publish_upload_pending(cfg, self.piece_pending_count());
                        return Ok(SendOutcome {
                            kind: OutProgress::RateLimited(wait),
                            wrote: any,
                        });
                    }
                    inf.rate_reserved = need;
                }
            }
            match crate::upload::write_framed_piece(stream, inf, scratch).await {
                Ok(payload) => {
                    let reserved = if let Some(Active::Piece(i)) = self.active.take() {
                        i.rate_reserved
                    } else {
                        0
                    };
                    any = true;
                    if reserved > payload {
                        if let Some(lim) = cfg.wire_limiter.as_ref() {
                            lim.refund_upload(reserved - payload);
                        }
                    }
                    if let Some(ref c) = cfg.wire_up {
                        c.fetch_add(payload, Ordering::Relaxed);
                    }
                    if let Some(ref cb) = cfg.on_upload {
                        cb(torrent.id, payload);
                    }
                    continue;
                }
                Err(e) => {
                    // write_all may have put partial bytes on the wire; never
                    // abort/refund/keep-peer mid-frame (same as ctrl write path).
                    publish_upload_pending(cfg, self.piece_pending_count());
                    return Err(Error::Msg(format!(
                        "upload write failed piece={} begin={}: {e}",
                        block.index, block.begin
                    )));
                }
            }
        }
        publish_upload_pending(cfg, self.piece_pending_count());
        Ok(if any {
            SendOutcome {
                kind: OutProgress::Progress,
                wrote: true,
            }
        } else {
            SendOutcome::idle()
        })
    }
}

impl Default for OutQueue {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upload::UploadBlock;

    fn block(index: u32, begin: u32) -> UploadBlock {
        UploadBlock {
            index,
            begin,
            length: 16 * 1024,
        }
    }

    #[test]
    fn can_abort_matrix() {
        let b = block(0, 0);
        let plain0 = InFlightUpload::test_buffered(b, false, 0, 100);
        assert!(plain0.can_abort());
        let cipher0 = InFlightUpload::test_buffered(b, true, 0, 100);
        assert!(!cipher0.can_abort());
        let plain_sent = InFlightUpload::test_buffered(b, false, 10, 100);
        assert!(!plain_sent.can_abort());
        let cipher_sent = InFlightUpload::test_buffered(b, true, 10, 100);
        assert!(!cipher_sent.can_abort());
    }

    #[test]
    fn cancel_queued_piece() {
        let mut q = OutQueue::new();
        let b = block(1, 0);
        assert!(q.try_push_piece(b));
        assert!(q.cancel_piece(b));
        assert_eq!(q.piece_pending_count(), 0);
    }

    #[test]
    fn cancel_active_abortable() {
        let mut q = OutQueue::new();
        let b = block(2, 0);
        q.active = Some(Active::Piece(InFlightUpload::test_buffered(
            b, false, 0, 50,
        )));
        assert!(q.cancel_piece(b));
        assert!(q.active.is_none());
    }

    #[test]
    fn cancel_active_cipher_applied_keeps_active() {
        let mut q = OutQueue::new();
        let b = block(3, 0);
        q.active = Some(Active::Piece(InFlightUpload::test_buffered(b, true, 0, 50)));
        assert!(!q.cancel_piece(b));
        assert!(matches!(q.active, Some(Active::Piece(_))));
        assert!(q.has_unabortable_piece());
    }

    #[test]
    fn clear_pieces_keeps_cipher_active() {
        let mut q = OutQueue::new();
        let b = block(4, 0);
        assert!(q.try_push_piece(block(5, 0)));
        q.active = Some(Active::Piece(InFlightUpload::test_buffered(b, true, 0, 50)));
        q.clear_pieces();
        assert_eq!(q.pieces.len(), 0);
        assert!(matches!(q.active, Some(Active::Piece(ref inf)) if inf.block == b));
    }

    #[test]
    fn clear_pieces_drops_abortable_active() {
        let mut q = OutQueue::new();
        let b = block(6, 0);
        q.active = Some(Active::Piece(InFlightUpload::test_buffered(
            b, false, 0, 50,
        )));
        q.clear_pieces();
        assert!(q.active.is_none());
    }

    #[test]
    fn only_one_keepalive_queued() {
        let mut q = OutQueue::new();
        let mut encode = super::super::ctrl_scratch::CtrlScratch::new();
        q.push_keepalive(&mut encode);
        q.push_keepalive(&mut encode);
        q.push_keepalive(&mut encode);
        assert_eq!(q.ctrl.len(), 1);
        assert!(q.keepalive_queued);
        assert_eq!(q.ctrl[0].len(), 4);
    }

    #[test]
    fn drop_out_queue_empty() {
        drop(OutQueue::new());
    }
}
