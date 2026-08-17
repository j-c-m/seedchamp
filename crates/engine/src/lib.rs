//! seedchamp engine: catalog, disk, crypto, wire, session, peer, runtime.
//!
//! See `docs/design.md` and `docs/domains.md`.

#![deny(unsafe_code)]

pub mod activity_log;
pub mod bench;
pub mod bencode;
pub mod catalog;
pub mod config;
pub mod control;
pub mod crypto;
pub mod disk;
pub mod error;
pub mod hash;
pub mod hot;
pub mod library;
pub mod metainfo;
pub mod net;
pub mod peer;
pub mod process_metrics;
pub mod rate_limit;
pub mod runtime;
pub mod session;
pub mod staging;
pub mod tracker;
pub mod upload;
pub mod wire;

pub use activity_log::{init_activity_logging, ActivityLog, ActivityLogLayer, LogLine};
pub use bench::{
    bench_catalog_fill_and_list, bench_list_existing, current_rss_bytes, print_report, BenchReport,
};
pub use catalog::{
    Catalog, FileProgress, FileRow, InsertOutcome, SessionLimits, StorageAuditReport,
    TorrentDetail, TorrentInsert, TorrentListRow, TrackerAnnounceUpdate, TrackerRow,
};
pub use config::{
    default_config_path, default_data_dir, default_sort_screens, load as load_config,
    load_file as load_config_file, resolve_config_path, to_toml_string,
    write_template as write_config_template, CatalogConfig, Config, LimitsConfig, NetworkConfig,
    PathsConfig, StartupCatalogReport, SwarmConfig, TuiConfig, TuiSortScreen, WatchConfig,
    WatchDirConfig,
};
pub use control::{
    spawn_control_plane, ControlEvent, ControlHandle, ControlPlane, EngineCommand, RuntimeInfo,
};
pub use crypto::{EncryptionMode, MseSession, Rc4, CRYPTO_PLAIN, CRYPTO_RC4};
pub use disk::{
    check_complete_layout, ensure_storage, expand_user_path, open_read_compio_peer,
    relocate_torrent_data, with_peer_fd_cache, write_piece, FdCache, FileLayout, IoSpan,
    StorageFileProblem, StorageLayout, StorageProblemKind,
};
pub use error::{Error, Result};
pub use hash::{recheck_torrent, recheck_torrent_with_progress, RecheckProgress, RecheckReport};
pub use hot::{HotRegistry, HotTorrent};
pub use library::{
    add_torrent, add_torrent_bytes, choose_placement, date_stamp, default_ltep_client,
    expand_dl_path_template, free_space_bytes, generate_peer_id, generate_peer_id_with_prefix,
    leech_cache_enabled, load_torrent_bytes, pkg_version_major, poll_watch_once, resolve_dl_path,
    resolve_ltep_client, resolve_peer_id_prefix, run_serve_loop, sanitize_path_component,
    serve_main, spawn_watcher, wanted_bytes_from_layout, wanted_bytes_from_metainfo, AddOptions,
    AddReport, DlPathContext, Placement, SeedHandle, WatchCallback, WatchHandle, WatchLoadEvent,
    DEFAULT_PEER_ID_PREFIX, PKG_VERSION,
};
pub use metainfo::{normalize_path_component, Metainfo};
pub use peer::{run_inbound_peer, run_outbound_peer, PeerConfig};
pub use process_metrics::{
    collect_filesystem_usage, FilesystemUsage, ProcessSample, ProcessSampleState, ThreadGroup,
};
pub use rate_limit::WireRateLimiter;
pub use runtime::{
    adapt_pipeline, clamp_initial_pipeline, default_hash_workers, default_peer_workers,
    desired_pipeline_blocks, recheck_torrent_with_pool, DiskBackendKind, DiskWorker, HashPool,
    PeerWorkerPool, PipelineAdaptOutcome, PipelineAdaptState, PipelineTuning, DEFAULT_DISK_DEPTH,
    DEFAULT_PIPELINE, DISK_WORKER_DEAD_STATUS, MAX_DISK_RESTARTS, MAX_PIPELINE, MIN_PIPELINE,
    REQUEST_QUEUE_TIME_SECS,
};
pub use session::{
    PeerCrypto, PeerDirection, PeerInfo, RelocateKind, RelocateReport, RuntimeConfig,
    SessionRuntime, SessionSnapshot, TorrentLive,
};
pub use staging::{PieceBufferPool, StagingPool, BLOCK_SIZE, DEFAULT_STAGING_MEM_LIMIT};
pub use tracker::{announce, announce_limited, tracker_host_key, tracker_user_agent, HostLimiter};
pub use upload::{
    begin_upload, write_framed_piece, InFlightUpload, ResolvedUploadBackend, UploadBackend,
    UploadBlock, UploadOptions, MAX_UPLOAD_REQQ,
};
pub use wire::{identify_peer_id, ltep_client_version, prefer_client_label};

/// Crate version with short git revision (`0.1.0-abc1234`, or `-dirty` if tree dirty).
///
/// `GIT_SHA` is set by `build.rs` (`unknown` if git is unavailable).
pub const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "-", env!("GIT_SHA"));
