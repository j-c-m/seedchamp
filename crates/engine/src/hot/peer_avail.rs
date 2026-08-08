//! Per-connection contribution to swarm piece availability.

use std::sync::Arc;

use crate::catalog::{bitfield_get, bitfield_set, empty_bitfield};

use super::HotTorrent;

/// Tracks one connection's bitfield contribution to [`HotTorrent`] availability.
pub struct PeerAvailability {
    torrent: Arc<HotTorrent>,
    bf: Vec<u8>,
}

impl PeerAvailability {
    pub fn new(torrent: Arc<HotTorrent>) -> Self {
        let pc = torrent.piece_count;
        Self {
            torrent,
            bf: empty_bitfield(pc),
        }
    }

    pub fn on_bitfield(&mut self, bf: &[u8]) {
        self.torrent.avail_sub_bitfield(&self.bf);
        let n = self.bf.len().min(bf.len());
        self.bf.fill(0);
        self.bf[..n].copy_from_slice(&bf[..n]);
        self.torrent.avail_add_bitfield(&self.bf);
    }

    pub fn on_have(&mut self, index: u32) {
        if index >= self.torrent.piece_count || bitfield_get(&self.bf, index) {
            return;
        }
        bitfield_set(&mut self.bf, index);
        self.torrent.avail_inc(index);
    }
}

impl Drop for PeerAvailability {
    fn drop(&mut self) {
        self.torrent.avail_sub_bitfield(&self.bf);
    }
}
