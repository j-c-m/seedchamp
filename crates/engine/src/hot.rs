//! Hot set: in-memory active torrent aggregate.
//!
//! **Invariants**
//! - Only torrents with `want_start` / live peers / active leech hold a [`HotTorrent`].
//! - `wanted_bf` + `download_missing` reflect priority-filtered need (O(1) dial/stop).
//! - `in_flight` claims prevent multi-peer pile-up on the same piece index (until endgame).
//! - `have_hub` fans verified piece indices to peer sessions for BEP3 HAVE.
//! - Piece hashes may be dropped after full-torrent complete (seed path only needs bitfield).
//!
//! **Locking (`parking_lot` task-fair RwLock)**
//! - Do **not** re-enter `pieces` / `wanted_bf` / `availability` / `in_flight` while already
//!   holding any of them. Nested `read()` while a `write()` waits deadlocks the thread
//!   (writers block new readers; the outer read never releases).
//! - Under lock: copy bitfields / counters; release before scan, callback, or second hot lock.
//!   Long holds stall `mark_have` and park peer I/O workers.
//! - `pick_rarest_piece` snapshots then drops **before** the eligible walk and `try_claim`.
//! - Never hold `pieces` while taking `wanted_bf` write (or the reverse).
//! - `download_missing` / `have_count` atomics are lock-free for TUI snapshots; use them
//!   instead of re-locking `pieces` while leeching (mark_have writes `pieces` continuously).
//! - `layout` is `RwLock<Arc<…>>`: **read** freely for I/O; **write** only for live
//!   data_root handoff. Never hold `layout` write while taking catalog/registry locks
//!   (handoff order: copy → catalog → layout write → delete temp, no other hot locks).
//!
//! Catalog column `want_start` means “user wants this torrent active in the swarm.”

mod have;
mod peer_avail;
mod pieces;
mod registry;

#[cfg(test)]
mod tests;

pub use peer_avail::PeerAvailability;
pub use registry::{load_tracker_tiers, HotRegistry};

use std::collections::HashSet;
use std::sync::atomic::{AtomicU32, AtomicU64};
use std::sync::Arc;

use flume::{Receiver as FlumeReceiver, Sender as FlumeSender};
use parking_lot::{Mutex, RwLock};

use crate::catalog::{all_set_bitfield, empty_bitfield};
use crate::disk::StorageLayout;

/// Mutable piece possession for a hot torrent.
#[derive(Clone, Debug)]
pub struct PieceState {
    pub bitfield: Vec<u8>,
    pub complete: bool,
    pub have_count: u32,
}

/// In-memory torrent ready to seed and/or leech.
pub struct HotTorrent {
    pub id: i64,
    pub infohash: [u8; 20],
    pub name: String,
    /// Shared storage map — hash/disk jobs clone the Arc, not thousands of paths.
    /// `RwLock` so leech_cache handoff can swap `data_root` without stopping peers.
    layout: RwLock<Arc<StorageLayout>>,
    pub piece_count: u32,
    /// 20 * piece_count SHA-1 hashes (needed for leech verify).
    /// Dropped after full-torrent complete (seed path only needs bitfield).
    pub piece_hashes: RwLock<Arc<Vec<u8>>>,
    pub pieces: RwLock<PieceState>,
    /// Parallel to `layout.files`: `0` = off, `≥1` = download. Live-updatable.
    pub file_priority: RwLock<Vec<i32>>,
    /// Tracker tiers `(tier, urls)` cached at activate — announce must not reopen SQLite.
    pub tracker_tiers: Vec<(i64, Vec<String>)>,
    /// rtorrent-style announce key (stable for this torrent; never 0 when loaded).
    pub tracker_key: u32,
    /// Bit per piece: intersects any priority>0 file. Rebuilt on priority change.
    /// Makes `wants_piece` O(1) — critical for multi‑GB multi‑file torrents.
    wanted_bf: RwLock<Vec<u8>>,
    /// Wanted pieces we still need. O(1) dial/leech stop.
    /// Full-torrent `pieces.complete` is separate — off files may remain missing forever.
    download_missing: AtomicU32,
    /// Full-torrent have piece count (mirrors `PieceState.have_count`).
    /// Lock-free for TUI / possession messages; updated under `pieces` write.
    have_count_atomic: AtomicU32,
    /// Payload bytes covered by have pieces (full torrent). Tracker `downloaded`
    /// uses this with a per-start baseline (rtorrent `completed_adjusted`).
    /// Tracker `left` is `layout.total_size − completed_payload` (BitTorrent:
    /// full-torrent remaining = size − completed; not priority-filtered).
    completed_payload: AtomicU64,
    /// Pieces exclusively reserved by one leech peer (not yet have). Prevents every
    /// connection from sequentially starting the same piece indices and downloading
    /// the torrent N times over the wire. Ignored in true endgame (see
    /// [`Self::try_claim_piece`]).
    in_flight: RwLock<HashSet<u32>>,
    /// How many **connected** peers advertise each piece (for rarest-first).
    /// Updated by leech sessions on bitfield/HAVE; decremented on disconnect.
    availability: RwLock<Vec<u16>>,
    /// Live peer sessions subscribe here; [`Self::mark_have`] fans out piece indices
    /// so every connection can send BEP3 HAVE (not only the initial bitfield).
    have_hub: HaveHub,
    /// Shared leech piece buffers (byte budget). `None` when seed-only or wanted-complete.
    staging_pool: RwLock<Option<Arc<crate::staging::PieceBufferPool>>>,
    /// Configured staging RAM limit (bytes) for [`Self::ensure_staging_pool`].
    staging_mem_limit: AtomicU64,
}

/// Fan-out of newly verified piece indices to peer sessions on this torrent.
struct HaveHub {
    subs: Mutex<Vec<FlumeSender<u32>>>,
}

impl HaveHub {
    fn new() -> Self {
        Self {
            subs: Mutex::new(Vec::new()),
        }
    }

    fn subscribe(&self) -> FlumeReceiver<u32> {
        let (tx, rx) = flume::unbounded();
        self.subs.lock().push(tx);
        rx
    }

    fn publish(&self, index: u32) {
        let mut subs = self.subs.lock();
        subs.retain(|tx| tx.send(index).is_ok());
    }

    #[cfg(test)]
    fn subscriber_count(&self) -> usize {
        self.subs.lock().len()
    }
}

impl HotTorrent {
    fn priorities_from_layout(layout: &StorageLayout) -> Vec<i32> {
        layout.files.iter().map(|f| f.priority).collect()
    }

    /// Clone the current layout Arc (short read lock).
    #[inline]
    pub fn layout(&self) -> Arc<StorageLayout> {
        self.layout.read().clone()
    }

    /// Live data_root swap for leech_cache handoff. Returns previous root.
    ///
    /// Callers must not hold catalog or other hot locks that handoff takes.
    pub fn set_data_root_live(&self, new_root: std::path::PathBuf) -> std::path::PathBuf {
        let mut g = self.layout.write();
        let old = g.data_root.clone();
        if old == new_root {
            return old;
        }
        let mut lay = (**g).clone();
        lay.data_root = new_root;
        *g = Arc::new(lay);
        old
    }

    fn finish_new(self) -> Self {
        self.rebuild_wanted_and_missing();
        self.recount_completed_payload();
        self
    }

    /// Construct a complete seeder torrent (tests / harness).
    pub fn new_complete(
        id: i64,
        infohash: [u8; 20],
        name: String,
        layout: StorageLayout,
        piece_hashes: Vec<u8>,
    ) -> Self {
        let piece_count = layout.piece_count;
        let file_priority = Self::priorities_from_layout(&layout);
        // Hashes optional for pure seed tests; keep if provided.
        let hashes = if piece_hashes.len() == piece_count as usize * 20 {
            piece_hashes
        } else {
            Vec::new()
        };
        Self {
            id,
            infohash,
            name,
            layout: RwLock::new(Arc::new(layout)),
            piece_count,
            piece_hashes: RwLock::new(Arc::new(hashes)),
            pieces: RwLock::new(PieceState {
                bitfield: all_set_bitfield(piece_count),
                complete: true,
                have_count: piece_count,
            }),
            file_priority: RwLock::new(file_priority),
            tracker_tiers: Vec::new(),
            tracker_key: crate::tracker::generate_tracker_key(),
            wanted_bf: RwLock::new(empty_bitfield(piece_count)),
            download_missing: AtomicU32::new(0),
            have_count_atomic: AtomicU32::new(piece_count),
            completed_payload: AtomicU64::new(0),
            in_flight: RwLock::new(HashSet::new()),
            availability: RwLock::new(vec![0u16; piece_count as usize]),
            have_hub: HaveHub::new(),
            staging_pool: RwLock::new(None),
            staging_mem_limit: AtomicU64::new(crate::staging::DEFAULT_STAGING_MEM_LIMIT),
        }
        .finish_new()
    }

    /// Construct an empty leecher torrent (no pieces have).
    pub fn new_empty(
        id: i64,
        infohash: [u8; 20],
        name: String,
        layout: StorageLayout,
        piece_hashes: Vec<u8>,
    ) -> Self {
        let piece_count = layout.piece_count;
        let file_priority = Self::priorities_from_layout(&layout);
        Self {
            id,
            infohash,
            name,
            layout: RwLock::new(Arc::new(layout)),
            piece_count,
            piece_hashes: RwLock::new(Arc::new(piece_hashes)),
            pieces: RwLock::new(PieceState {
                bitfield: empty_bitfield(piece_count),
                complete: false,
                have_count: 0,
            }),
            file_priority: RwLock::new(file_priority),
            tracker_tiers: Vec::new(),
            tracker_key: crate::tracker::generate_tracker_key(),
            wanted_bf: RwLock::new(empty_bitfield(piece_count)),
            download_missing: AtomicU32::new(0),
            have_count_atomic: AtomicU32::new(0),
            completed_payload: AtomicU64::new(0),
            in_flight: RwLock::new(HashSet::new()),
            availability: RwLock::new(vec![0u16; piece_count as usize]),
            have_hub: HaveHub::new(),
            staging_pool: RwLock::new(None),
            staging_mem_limit: AtomicU64::new(crate::staging::DEFAULT_STAGING_MEM_LIMIT),
        }
        .finish_new()
    }

    /// Subscribe to piece indices that become have (for wire HAVE messages).
    ///
    /// Drop the receiver (or the peer task) to unsubscribe; dead senders are
    /// pruned on the next publish.
    pub fn subscribe_have(&self) -> FlumeReceiver<u32> {
        self.have_hub.subscribe()
    }
}
