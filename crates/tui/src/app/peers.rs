//! Peers screen selection and sorted peer list.

use super::Mode;

impl super::App {
    pub fn open_peers(&mut self) {
        self.mode = Mode::Peers;
        // Fresh visit: start at top of the sorted peer list.
        self.peer_selected_id = None;
        self.peer_scroll = 0;
        let peers = self.peers_for_screen();
        if let Some(p) = peers.first() {
            self.peer_selected_id = Some(p.id);
        }
    }

    /// Sorted peer list for the peers screen (same order as the table).
    pub fn peers_for_screen(&self) -> Vec<seedchamp_engine::PeerInfo> {
        let sel_id = self.selected_id();
        let mut peers: Vec<_> = self.snap.peers.iter().cloned().collect();
        if let Some(id) = sel_id {
            peers.retain(|p| p.torrent_id == id);
        }
        peers.sort_by(|a, b| {
            let ba = a.download_bps.max(a.upload_bps);
            let bb = b.download_bps.max(b.upload_bps);
            bb.cmp(&ba)
                .then_with(|| b.upload_bps.cmp(&a.upload_bps))
                .then_with(|| b.download_bps.cmp(&a.download_bps))
                .then_with(|| a.id.cmp(&b.id))
        });
        peers
    }

    /// Move peers-screen cursor by `delta` (clamped, no wrap).
    pub fn peer_select_delta(&mut self, delta: i32) {
        let peers = self.peers_for_screen();
        if peers.is_empty() {
            self.peer_selected_id = None;
            self.peer_scroll = 0;
            return;
        }
        let n = peers.len() as i32;
        let cur = self
            .peer_selected_id
            .and_then(|id| peers.iter().position(|p| p.id == id))
            .unwrap_or(0) as i32;
        let next = (cur + delta).clamp(0, n - 1) as usize;
        self.peer_selected_id = Some(peers[next].id);
        // Keep selection on-screen once draw knows view height; clamp scroll here.
        if next < self.peer_scroll {
            self.peer_scroll = next;
        }
    }

    pub fn peer_select_first(&mut self) {
        let peers = self.peers_for_screen();
        self.peer_scroll = 0;
        self.peer_selected_id = peers.first().map(|p| p.id);
    }

    pub fn peer_select_last(&mut self) {
        let peers = self.peers_for_screen();
        if let Some(p) = peers.last() {
            self.peer_selected_id = Some(p.id);
            // draw will pull scroll down so last row is visible
            self.peer_scroll = peers.len().saturating_sub(1);
        }
    }
}
