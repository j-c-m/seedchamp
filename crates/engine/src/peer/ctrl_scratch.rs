//! Per-peer reusable buffer for encoding control messages (Request/Have/…).

use crate::wire::{
    append_cancel, append_have, append_interested, append_keepalive, append_not_interested,
    append_reject_request, append_request, WIRE_HAVE, WIRE_REQUEST,
};

/// Session-local encode buffer: clear → append frames → [`Self::take`] for the queue.
///
/// After `take`, capacity is re-reserved so the next batch does not start from zero.
pub(crate) struct CtrlScratch {
    buf: Vec<u8>,
}

impl CtrlScratch {
    /// Default capacity ≈ 32 Request frames (pipeline-friendly).
    pub(crate) fn new() -> Self {
        Self {
            buf: Vec::with_capacity(32 * WIRE_REQUEST),
        }
    }

    #[inline]
    pub(crate) fn clear(&mut self) {
        self.buf.clear();
    }

    #[inline]
    pub(crate) fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Hand off encoded bytes; re-reserve for the next encode.
    pub(crate) fn take(&mut self) -> Vec<u8> {
        let cap = self.buf.capacity().max(32 * WIRE_REQUEST);
        std::mem::replace(&mut self.buf, Vec::with_capacity(cap))
    }

    pub(crate) fn append_keepalive(&mut self) {
        append_keepalive(&mut self.buf);
    }

    pub(crate) fn append_interested(&mut self) {
        append_interested(&mut self.buf);
    }

    pub(crate) fn append_not_interested(&mut self) {
        append_not_interested(&mut self.buf);
    }

    pub(crate) fn append_have(&mut self, index: u32) {
        append_have(&mut self.buf, index);
    }

    pub(crate) fn append_request(&mut self, index: u32, begin: u32, length: u32) {
        append_request(&mut self.buf, index, begin, length);
    }

    pub(crate) fn append_cancel(&mut self, index: u32, begin: u32, length: u32) {
        append_cancel(&mut self.buf, index, begin, length);
    }

    pub(crate) fn append_reject_request(&mut self, index: u32, begin: u32, length: u32) {
        append_reject_request(&mut self.buf, index, begin, length);
    }

    /// Reserve space for `n` HAVE messages before a burst.
    pub(crate) fn reserve_haves(&mut self, n: usize) {
        self.buf.reserve(n.saturating_mul(WIRE_HAVE));
    }

    /// Reserve space for `n` Request/Cancel frames.
    pub(crate) fn reserve_requests(&mut self, n: usize) {
        self.buf.reserve(n.saturating_mul(WIRE_REQUEST));
    }
}

impl Default for CtrlScratch {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{encode_message, Message};

    #[test]
    fn request_matches_encode_message() {
        let mut s = CtrlScratch::new();
        s.append_request(1, 2, 16384);
        let got = s.take();
        let expect = encode_message(&Message::Request {
            index: 1,
            begin: 2,
            length: 16384,
        });
        assert_eq!(got, expect);
    }

    #[test]
    fn have_cancel_reject_keepalive_match() {
        let mut s = CtrlScratch::new();
        s.append_have(9);
        assert_eq!(s.take(), encode_message(&Message::Have(9)));
        s.append_cancel(1, 0, 16);
        assert_eq!(
            s.take(),
            encode_message(&Message::Cancel {
                index: 1,
                begin: 0,
                length: 16
            })
        );
        s.append_reject_request(2, 0, 32);
        assert_eq!(
            s.take(),
            encode_message(&Message::RejectRequest {
                index: 2,
                begin: 0,
                length: 32
            })
        );
        s.append_keepalive();
        assert_eq!(s.take(), encode_message(&Message::KeepAlive));
        s.append_interested();
        assert_eq!(s.take(), encode_message(&Message::Interested));
        s.append_not_interested();
        assert_eq!(s.take(), encode_message(&Message::NotInterested));
    }
}
