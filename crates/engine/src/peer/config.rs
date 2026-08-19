//! Unified full-duplex peer session configuration.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;

use crate::crypto::EncryptionMode;
use crate::rate_limit::WireRateLimiter;
use crate::upload::UploadOptions;

use crate::runtime::HashPool;
use crate::runtime::{DEFAULT_PIPELINE, MAX_PIPELINE};
use crate::staging::DEFAULT_STAGING_MEM_LIMIT;

/// Full-duplex async peer session options (inbound + outbound).
#[derive(Clone)]
pub struct PeerConfig {
    pub peer_id: [u8; 20],
    pub encryption: EncryptionMode,
    pub upload: UploadOptions,
    /// Serve Request/Cancel (seed / seed-while-leech).
    pub allow_upload: bool,
    /// Issue Interested + Request pipeline when wanted pieces are missing.
    pub allow_download: bool,
    /// Initial download pipeline depth (blocks) before rate adapts.
    pub pipeline: usize,
    /// Adaptive pipeline cap (blocks).
    pub pipeline_max: usize,
    /// Per-torrent leech piece-buffer budget (bytes). Shared with human sizes in config.
    pub staging_mem_limit: u64,
    pub hash: Option<Arc<HashPool>>,
    pub on_piece: Option<Arc<dyn Fn(i64, u32, u32) + Send + Sync>>,
    pub stop: Option<Arc<AtomicBool>>,
    /// Dropped sender on session/torrent cancel wakes idle duplex parks.
    pub stop_rx: Option<flume::Receiver<()>>,
    /// Inbound: after infohash binds (id, name). Return false to reject (max_peers).
    pub on_bound: Option<Arc<dyn Fn(i64, String) -> bool + Send + Sync>>,
    pub piece_count: Option<Arc<AtomicU32>>,
    pub wire_up: Option<Arc<AtomicU64>>,
    pub wire_down: Option<Arc<AtomicU64>>,
    /// Called with (torrent_id, bytes) after every successful upload block.
    pub on_upload: Option<Arc<dyn Fn(i64, u64) + Send + Sync>>,
    pub queue_outstanding: Option<Arc<AtomicU64>>,
    pub queue_target: Option<Arc<AtomicU64>>,
    pub peer_interested: Option<Arc<AtomicBool>>,
    /// Remote is choking us (cannot download Requests unless Allowed Fast).
    pub peer_choking: Option<Arc<AtomicBool>>,
    /// Outbound Interested still set (want download from this peer).
    pub am_interested: Option<Arc<AtomicBool>>,
    pub upload_pending: Option<Arc<AtomicU64>>,
    pub peer_have: Option<Arc<AtomicU32>>,
    pub crypto: Option<Arc<AtomicU8>>,
    /// Remote client label (peer_id guess, upgraded by LTEP `v` when present).
    pub client_label: Option<Arc<Mutex<String>>>,
    /// BEP 10 LTEP extended-handshake `v` we advertise to peers.
    /// Default: [`crate::library::default_ltep_client`] (`seedchamp <VERSION>`).
    pub ltep_client: String,
    /// Our listen port for LTEP `p` (libtorrent handshake `p` key).
    pub listen_port: u16,
    /// Close seed↔seed (both complete, no transfer) after this long. **Zero** = off.
    pub redundant_seed_idle: Duration,
    /// Close when there is no actual transfer for this long. **Zero** = off.
    /// Seed↔seed uses `redundant_seed_idle` instead when that timer is on.
    pub useless_peer_idle: Duration,
    /// `SO_SNDBUF` request; 0 = kernel default (outbound dial).
    pub send_buffer_bytes: u64,
    /// `SO_RCVBUF` request; 0 = kernel default (outbound dial).
    pub recv_buffer_bytes: u64,
    /// Global wire rate limiter (`0` caps = unlimited, free path).
    pub wire_limiter: Option<Arc<WireRateLimiter>>,
}

impl Default for PeerConfig {
    fn default() -> Self {
        Self {
            peer_id: [0; 20],
            encryption: EncryptionMode::PreferPlain,
            upload: UploadOptions::default(),
            allow_upload: true,
            allow_download: true,
            pipeline: DEFAULT_PIPELINE,
            pipeline_max: MAX_PIPELINE,
            staging_mem_limit: DEFAULT_STAGING_MEM_LIMIT,
            hash: None,
            on_piece: None,
            stop: None,
            stop_rx: None,
            on_bound: None,
            piece_count: None,
            wire_up: None,
            wire_down: None,
            on_upload: None,
            queue_outstanding: None,
            queue_target: None,
            peer_interested: None,
            peer_choking: None,
            am_interested: None,
            upload_pending: None,
            peer_have: None,
            crypto: None,
            client_label: None,
            ltep_client: crate::library::default_ltep_client(),
            listen_port: 6881,
            redundant_seed_idle: Duration::from_secs(15),
            useless_peer_idle: Duration::from_secs(60),
            send_buffer_bytes: 0,
            recv_buffer_bytes: 0,
            wire_limiter: None,
        }
    }
}
