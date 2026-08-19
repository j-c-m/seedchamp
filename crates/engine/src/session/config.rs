//! Session runtime configuration (assembled at the session boundary).

use std::net::SocketAddr;

use crate::crypto::EncryptionMode;
use crate::error::Result;
use crate::upload::UploadOptions;

#[derive(Clone)]
pub struct RuntimeConfig {
    pub listen: SocketAddr,
    pub encryption: EncryptionMode,
    pub upload: UploadOptions,
    pub announce: bool,
    /// Initial download pipeline depth (blocks) at connect / reopen.
    pub pipeline: usize,
    /// Adaptive pipeline cap (blocks).
    pub pipeline_max: usize,
    /// Per-torrent leech piece-buffer budget (bytes). Default 256 MiB.
    pub staging_mem_limit: u64,
    pub manual_peers: Vec<SocketAddr>,
    /// Peer I/O worker threads (event-loop threads). Default = CPU count.
    pub peer_workers: Option<usize>,
    /// Piece-hash worker threads. Default = CPU count.
    pub hash_workers: Option<usize>,
    /// After piece SHA-1 OK, skip durable `pwrite` (bench / discard-after-hash).
    /// Bitfield still advances; must not serve discarded data (upload forced off).
    /// Default false. Active torrents otherwise **always** seed-while-leech.
    pub discard_writes: bool,
    /// DiskWorker backend: `auto` | `thread` | `uring` | `aio`.
    pub disk_backend: String,
    /// Max in-flight DiskWorker piece jobs (default 32).
    pub disk_depth: usize,
    /// Useful-peer floor to chase while leeching (and while seeding if
    /// [`Self::seed_dial_peers`]). Clamped ≤ `max_peers`. Default 20.
    pub min_peers: usize,
    /// Max concurrent peers **per torrent** (inbound + outbound). Default 40.
    pub max_peers: usize,
    /// When true, complete torrents dial out / starve-announce to fill min_peers.
    /// Default false (inbound-only peer growth while seeding).
    pub seed_dial_peers: bool,
    /// Max live peer sessions process-wide (inbound + outbound). Default 2048.
    /// Inbound accepts are refused when `peers.len() >= max_connections` (B9).
    pub max_connections: usize,
    /// Close seed↔seed (both complete, idle) after this many seconds. **0** = off.
    pub redundant_seed_idle_secs: u64,
    /// Close peers with no actual transfer after this many seconds. **0** = off.
    pub useless_peer_idle_secs: u64,
    /// `SO_SNDBUF` request; 0 = kernel default.
    pub send_buffer_bytes: u64,
    /// `SO_RCVBUF` request; 0 = kernel default.
    pub recv_buffer_bytes: u64,
    /// Max in-flight announces per tracker host; 0 = unlimited.
    pub max_concurrent_per_host: u32,
    /// Stagger first announces when many torrents activate (ms between each).
    pub startup_stagger_ms: u64,
    /// Cap concurrent announce jobs (torrent-level). Bounds in-flight async
    /// announce tasks when many torrents share a host. **0** = unlimited (not recommended).
    pub max_inflight_announces: u32,
    /// Tracker `numwant` (peers requested per announce).
    pub numwant: u32,
    /// Azureus-style peer id prefix bytes (typically 8). Rest of 20 is random.
    /// Default fixed `-sc0001-` (seedchamp). Override via config / env.
    pub peer_id_prefix: Vec<u8>,
    /// HTTP User-Agent for tracker announces. Default `seedchamp/<pkg-version>`.
    pub http_user_agent: String,
    /// BEP 10 LTEP extended-handshake `v` (client version string).
    /// Default `seedchamp <VERSION>`; overridable independently of peer id.
    pub ltep_client: String,
    /// Global wire upload cap (bytes/sec); **0 = unlimited**.
    pub max_upload_bps: u64,
    /// Global wire download cap (bytes/sec); **0 = unlimited**.
    pub max_download_bps: u64,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0:6881".parse().unwrap(),
            encryption: EncryptionMode::PreferPlain,
            upload: UploadOptions::default(),
            announce: true,
            pipeline: crate::runtime::DEFAULT_PIPELINE,
            pipeline_max: crate::runtime::MAX_PIPELINE,
            staging_mem_limit: crate::staging::DEFAULT_STAGING_MEM_LIMIT,
            manual_peers: Vec::new(),
            peer_workers: None,
            hash_workers: None,
            discard_writes: false,
            disk_backend: "auto".into(),
            disk_depth: crate::runtime::DEFAULT_DISK_DEPTH,
            min_peers: 20,
            max_peers: 40,
            seed_dial_peers: false,
            max_connections: 2048,
            redundant_seed_idle_secs: 15,
            useless_peer_idle_secs: 60,
            send_buffer_bytes: 0,
            recv_buffer_bytes: 0,
            max_concurrent_per_host: 2,
            startup_stagger_ms: 50,
            max_inflight_announces: 16,
            numwant: 50,
            peer_id_prefix: crate::library::DEFAULT_PEER_ID_PREFIX.to_vec(),
            http_user_agent: crate::tracker::tracker_user_agent().into(),
            ltep_client: crate::library::default_ltep_client(),
            max_upload_bps: 0,
            max_download_bps: 0,
        }
    }
}

impl RuntimeConfig {
    /// Build runtime config from process [`crate::config::Config`] (no manual peers).
    ///
    /// Lives here so **`config` does not depend on session implementation** —
    /// file/env config stays pure; the session boundary assembles wire/runtime knobs.
    pub fn from_config(cfg: &crate::config::Config) -> Result<Self> {
        let (min_peers, max_peers) = cfg.limits.clamped_peer_limits();
        Ok(Self {
            listen: cfg.listen_addr()?,
            encryption: cfg.encryption_mode()?,
            upload: cfg.upload_options()?,
            announce: cfg.network.announce,
            pipeline: crate::runtime::clamp_initial_pipeline(
                cfg.swarm.pipeline.max(1),
                cfg.swarm.pipeline_max.max(1),
            ),
            pipeline_max: cfg.swarm.pipeline_max.max(crate::runtime::MIN_PIPELINE),
            staging_mem_limit: {
                let n = cfg.swarm.staging_mem_limit;
                if n == 0 {
                    crate::staging::DEFAULT_STAGING_MEM_LIMIT
                } else {
                    n
                }
            },
            manual_peers: Vec::new(),
            peer_workers: cfg.peer_workers_opt(),
            hash_workers: cfg.hash_workers_opt(),
            discard_writes: false,
            disk_backend: {
                let b = cfg.disk.backend.trim();
                if b.is_empty() {
                    "auto".into()
                } else {
                    b.to_ascii_lowercase()
                }
            },
            disk_depth: cfg.disk.depth.clamp(1, 256),
            min_peers: min_peers as usize,
            max_peers: max_peers as usize,
            seed_dial_peers: cfg.limits.seed_dial_peers,
            max_connections: cfg.limits.max_connections.max(1) as usize,
            redundant_seed_idle_secs: cfg.limits.redundant_seed_idle_secs,
            useless_peer_idle_secs: cfg.limits.useless_peer_idle_secs,
            send_buffer_bytes: cfg.network.send_buffer_bytes,
            recv_buffer_bytes: cfg.network.recv_buffer_bytes,
            max_concurrent_per_host: cfg.tracker.max_concurrent_per_host,
            startup_stagger_ms: cfg.tracker.startup_stagger_ms,
            max_inflight_announces: cfg.tracker.max_inflight_announces,
            numwant: cfg.tracker.numwant,
            peer_id_prefix: crate::library::resolve_peer_id_prefix(&cfg.network.peer_id_prefix),
            http_user_agent: {
                let ua = cfg.network.http_user_agent.trim();
                if ua.is_empty() {
                    crate::tracker::tracker_user_agent().into()
                } else {
                    ua.to_string()
                }
            },
            ltep_client: {
                let v = cfg.network.ltep_client.trim();
                if v.is_empty() {
                    crate::library::resolve_ltep_client(&cfg.network.peer_id_prefix)
                } else {
                    v.to_string()
                }
            },
            max_upload_bps: cfg.limits.max_upload_bps,
            max_download_bps: cfg.limits.max_download_bps,
        })
    }
}
