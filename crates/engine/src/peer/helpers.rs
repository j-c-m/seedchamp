//! Shared helpers for the unified peer session.

use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::time::Duration;

use flume::Receiver as FlumeReceiver;

use super::config::PeerConfig;
use super::ctrl_scratch::CtrlScratch;
use super::out_queue::OutQueue;
use crate::catalog::count_have_bits;
use crate::crypto::{MseSession, Rc4};
use crate::session::PeerCrypto;

/// If no PIECE arrives while unchoked for this long, Cancel + re-issue outstanding
/// requests. Socket read timeout alone is not enough: keepalives, HAVE, and
/// upload Request traffic reset the read timer without advancing the download.
///
/// **Do not** requeue on every short read timeout — that re-requests the full
/// pipeline while old requests are still live and multiplies wire download.
pub(crate) const REQUEST_STALL: Duration = Duration::from_secs(20);
/// Endgame: re-request missing blocks sooner so multi-source finishes snappily.
pub(crate) const REQUEST_STALL_ENDGAME: Duration = Duration::from_secs(4);
/// BEP3 idle KeepAlive (and NAT refresh) when neither side has written recently.
pub(crate) const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(120);

pub(crate) struct WireCrypto {
    pub encrypt: Rc4,
    pub decrypt: Rc4,
}

pub(crate) fn mse_to_wire(mse: MseSession) -> (Option<WireCrypto>, PeerCrypto) {
    if mse.rc4 {
        (
            Some(WireCrypto {
                encrypt: mse.encrypt,
                decrypt: mse.decrypt,
            }),
            PeerCrypto::Rc4,
        )
    } else {
        (None, PeerCrypto::PePlain)
    }
}

/// May send download Requests: unchoked, or choked with a non-empty Allowed Fast set (B3/B7).
#[inline]
pub(crate) fn can_request_from(
    peer_choking: bool,
    allowed_fast: &std::collections::HashSet<u32>,
) -> bool {
    !peer_choking || !allowed_fast.is_empty()
}

/// Stored Suggest Piece indices per peer (BEP 6; newest first).
pub(crate) const SUGGEST_CAP: usize = 32;

/// Record a Suggest Piece index. Newest at the front. Out-of-range is ignored.
pub(crate) fn push_suggest(suggested: &mut VecDeque<u32>, index: u32, piece_count: u32) {
    if index >= piece_count {
        return;
    }
    if let Some(pos) = suggested.iter().position(|&j| j == index) {
        suggested.remove(pos);
    }
    suggested.push_front(index);
    while suggested.len() > SUGGEST_CAP {
        suggested.pop_back();
    }
}

/// First suggested index that `eligible` accepts. Drops the rest that fail.
pub(crate) fn take_suggested(
    suggested: &mut VecDeque<u32>,
    mut eligible: impl FnMut(u32) -> bool,
) -> Option<u32> {
    while let Some(i) = suggested.pop_front() {
        if eligible(i) {
            return Some(i);
        }
    }
    None
}

/// Drain the torrent HAVE hub into the outbound queue (non-blocking).
///
/// Returns true if any HAVE bytes were enqueued.
pub(crate) fn enqueue_have_messages(
    out: &mut OutQueue,
    scratch: &mut CtrlScratch,
    have_rx: &mut FlumeReceiver<u32>,
) -> bool {
    enqueue_have_messages_from(out, scratch, have_rx, None)
}

/// Like [`enqueue_have_messages`], including a piece index already received.
pub(crate) fn enqueue_have_messages_from(
    out: &mut OutQueue,
    scratch: &mut CtrlScratch,
    have_rx: &mut FlumeReceiver<u32>,
    first: Option<u32>,
) -> bool {
    scratch.clear();
    if let Some(index) = first {
        scratch.reserve_haves(1);
        scratch.append_have(index);
    }
    while let Ok(index) = have_rx.try_recv() {
        scratch.append_have(index);
    }
    if scratch.is_empty() {
        return false;
    }
    out.push_ctrl_owned(scratch.take());
    true
}

pub(crate) fn publish_peer_have(cfg: &PeerConfig, peer_bf: &[u8], piece_count: u32) {
    if let Some(ref a) = cfg.peer_have {
        a.store(count_have_bits(peer_bf, piece_count), Ordering::Relaxed);
    }
}

pub(crate) fn publish_upload_pending(cfg: &PeerConfig, n: usize) {
    if let Some(ref a) = cfg.upload_pending {
        a.store(n as u64, Ordering::Relaxed);
    }
}

pub(crate) fn publish_peer_choking(cfg: &PeerConfig, choking: bool) {
    if let Some(ref a) = cfg.peer_choking {
        a.store(choking, Ordering::Relaxed);
    }
}

pub(crate) fn publish_am_interested(cfg: &PeerConfig, interested: bool) {
    if let Some(ref a) = cfg.am_interested {
        a.store(interested, Ordering::Relaxed);
    }
}

pub(crate) fn publish_dl_queue(cfg: &PeerConfig, outstanding: u64, target: u64) {
    if let Some(ref q) = cfg.queue_outstanding {
        q.store(outstanding, Ordering::Relaxed);
    }
    if let Some(ref t) = cfg.queue_target {
        t.store(target, Ordering::Relaxed);
    }
}

pub(crate) fn clear_dl_queue(cfg: &PeerConfig) {
    publish_dl_queue(cfg, 0, 0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_suggest_ignores_out_of_range() {
        let mut q = VecDeque::new();
        push_suggest(&mut q, 5, 4);
        push_suggest(&mut q, 4, 4);
        assert!(q.is_empty());
        push_suggest(&mut q, 3, 4);
        assert_eq!(q, [3]);
    }

    #[test]
    fn push_suggest_newest_first_dedup_and_cap() {
        let mut q = VecDeque::new();
        for i in 0..SUGGEST_CAP as u32 {
            push_suggest(&mut q, i, 64);
        }
        assert_eq!(q.len(), SUGGEST_CAP);
        assert_eq!(q.front(), Some(&(SUGGEST_CAP as u32 - 1)));
        assert_eq!(q.back(), Some(&0));

        push_suggest(&mut q, 100, 128);
        assert_eq!(q.len(), SUGGEST_CAP);
        assert_eq!(q.front(), Some(&100));
        assert_eq!(q.back(), Some(&1), "oldest dropped");

        push_suggest(&mut q, 100, 128);
        assert_eq!(q.iter().filter(|&&i| i == 100).count(), 1);
        assert_eq!(q.front(), Some(&100));
    }

    #[test]
    fn take_suggested_skips_ineligible_and_keeps_order() {
        let mut q = VecDeque::from([3, 2, 1]);
        assert_eq!(take_suggested(&mut q, |i| i == 1), Some(1));
        assert!(q.is_empty(), "ineligible 3 and 2 dropped");
    }
}
