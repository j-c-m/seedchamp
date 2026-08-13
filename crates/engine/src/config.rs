//! Process configuration (TOML under XDG).
//!
//! **Install-first:** default config is `$XDG_CONFIG_HOME/seedchamp/config.toml`
//! (or `~/.config/seedchamp/config.toml`). Data defaults under
//! `$XDG_DATA_HOME/seedchamp/` (or `~/.local/share/seedchamp/`).
//!
//! **Precedence (highest first):** CLI flags → environment → config file → built-in defaults.
//!
//! **Limits:** config file is primary; applied into the catalog on startup so the
//! TUI/session reflect file values. Edit config (or use `config show`) as source of truth.

use std::env;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize};

use crate::catalog::SessionLimits;
use crate::crypto::EncryptionMode;
use crate::error::{Error, Result};
use crate::upload::{UploadBackend, UploadOptions};

/// Built-in defaults (no file, no env).
pub fn default_config() -> Config {
    Config::default()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub paths: PathsConfig,
    pub network: NetworkConfig,
    pub upload: UploadSection,
    pub swarm: SwarmConfig,
    /// Verified leech piece-write path ([`DiskWorker`](crate::runtime::DiskWorker)).
    pub disk: DiskConfig,
    pub limits: LimitsConfig,
    pub logging: LoggingConfig,
    pub tracker: TrackerConfig,
    /// Catalog maintenance (soft-delete purge, etc.).
    pub catalog: CatalogConfig,
    /// Directory watchers (rtorrent `schedule2 = watch_*` equivalent).
    pub watch: WatchConfig,
    /// Interactive TUI defaults (list sort / views).
    pub tui: TuiConfig,
}

impl Default for Config {
    fn default() -> Self {
        let data = default_data_dir();
        Self {
            paths: PathsConfig {
                db: data.join("catalog.sqlite"),
                data_root: data.join("downloads"),
                torrent_dir: data.join("torrents"),
                leech_cache: PathBuf::new(),
                leech_cache_size: 0,
            },
            network: NetworkConfig::default(),
            upload: UploadSection::default(),
            swarm: SwarmConfig::default(),
            disk: DiskConfig::default(),
            limits: LimitsConfig::default(),
            logging: LoggingConfig::default(),
            tracker: TrackerConfig::default(),
            catalog: CatalogConfig::default(),
            watch: WatchConfig::default(),
            tui: TuiConfig::default(),
        }
    }
}

/// SQLite catalog maintenance options.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CatalogConfig {
    /// Days after soft-delete before **catalog rows** are hard-removed on startup.
    ///
    /// - Default **30**.
    /// - **0** = never purge.
    /// - Payload / download files under `data_root` are **never** deleted by this.
    pub soft_delete_purge_days: u64,
}

impl Default for CatalogConfig {
    fn default() -> Self {
        Self {
            soft_delete_purge_days: 30,
        }
    }
}

/// One TUI list sort screen (key `1`/`2` or label; cycle with `o`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TuiSortScreen {
    /// Jump key / id (`"1"`, `"2"`, …) and aliases for `default_sort`.
    pub key: String,
    /// Short label in the list title (e.g. `rate`, `name`).
    pub label: String,
    /// Ordered sort criteria (first decisive wins). Known tokens:
    /// `off_first`, `down_rate_desc`, `up_rate_desc`, `added_desc`, `name_asc`, `id_asc`.
    pub order: Vec<String>,
}

impl Default for TuiSortScreen {
    fn default() -> Self {
        Self {
            key: "1".into(),
            label: "rate".into(),
            order: vec![
                "off_first".into(),
                "down_rate_desc".into(),
                "up_rate_desc".into(),
                "added_desc".into(),
                "name_asc".into(),
            ],
        }
    }
}

/// TUI list views and default sort.
///
/// Screens are config-driven (`tui.screens`); runtime `o` / `1` / `2` / `:sort`.
/// Theme colors live in a **separate** theme file; this field is only a pointer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TuiConfig {
    /// Default screen: key (`1`/`2`) or label (`rate`/`name`).
    pub default_sort: String,
    /// Sort screens (empty → built-in rate + name defaults).
    pub screens: Vec<TuiSortScreen>,
    /// Theme name (`default`, `soft`) or path to a theme TOML file.
    /// Resolved relative to the config directory (`themes/<name>.toml`).
    pub theme: String,
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            default_sort: "1".into(),
            screens: default_sort_screens(),
            theme: "default".into(),
        }
    }
}

/// Built-in screen 1 (rate, off first) + screen 2 (name).
pub fn default_sort_screens() -> Vec<TuiSortScreen> {
    vec![
        TuiSortScreen {
            key: "1".into(),
            label: "rate".into(),
            order: vec![
                "off_first".into(),
                "down_rate_desc".into(),
                "up_rate_desc".into(),
                "added_desc".into(),
                "name_asc".into(),
            ],
        },
        TuiSortScreen {
            key: "2".into(),
            label: "name".into(),
            order: vec!["name_asc".into(), "id_asc".into()],
        },
    ]
}

impl TuiConfig {
    /// Effective screens (never empty).
    pub fn sort_screens(&self) -> Vec<TuiSortScreen> {
        if self.screens.is_empty() {
            default_sort_screens()
        } else {
            self.screens.clone()
        }
    }

    /// Index into [`Self::sort_screens`] for [`Self::default_sort`].
    pub fn default_screen_index(&self) -> usize {
        let screens = self.sort_screens();
        let key = self.default_sort.trim().to_ascii_lowercase();
        screens
            .iter()
            .position(|s| {
                s.key.eq_ignore_ascii_case(&key)
                    || s.label.eq_ignore_ascii_case(&key)
                    || (key == "rate" && s.label.eq_ignore_ascii_case("rate"))
                    || (key == "name" && s.label.eq_ignore_ascii_case("name"))
            })
            .unwrap_or(0)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PathsConfig {
    /// SQLite catalog path.
    pub db: PathBuf,
    /// Default directory for torrent payload files (permanent library root).
    pub data_root: PathBuf,
    /// Where to save copies of .torrent metainfo (optional convenience).
    pub torrent_dir: PathBuf,
    /// Optional leech cache for incomplete wanted downloads that fit free space
    /// and the soft size cap.
    ///
    /// Empty = disabled. When set, wanted payload that fits is written under
    /// `{leech_cache}/{infohash}/…`; on wanted-complete, publish dest, swap
    /// catalog + live layout, delete stage. See `library::leech_cache`.
    ///
    /// Recommended on a fast local volume (typically SSD). Leave empty to write
    /// straight to the permanent data root.
    #[serde(default)]
    pub leech_cache: PathBuf,
    /// Soft max committed bytes under [`Self::leech_cache`] (`0` = no soft cap).
    ///
    /// Cap is checked from catalog reserved size of staged torrents
    /// (`home_root` set), not a recursive disk walk. TOML: integer or `"100G"`.
    /// Env: `SEEDCHAMP_LEECH_CACHE_SIZE`.
    #[serde(deserialize_with = "deserialize_byte_size_field", default)]
    pub leech_cache_size: u64,
}

impl Default for PathsConfig {
    fn default() -> Self {
        let data = default_data_dir();
        Self {
            db: data.join("catalog.sqlite"),
            data_root: data.join("downloads"),
            torrent_dir: data.join("torrents"),
            leech_cache: PathBuf::new(),
            leech_cache_size: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NetworkConfig {
    /// Bind address for peer listen (e.g. `0.0.0.0:6881`).
    pub listen: String,
    /// Wire encryption: off | prefer-plain | prefer-rc4 | require-rc4
    pub encryption: String,
    /// HTTP/UDP tracker announce enabled by default.
    pub announce: bool,
    /// Requested `SO_SNDBUF` size in bytes. `0` = kernel default.
    /// Kernel may clamp or double. Applied best-effort on each peer socket.
    pub send_buffer_bytes: u64,
    /// Requested `SO_RCVBUF` size in bytes. `0` = kernel default.
    /// Same clamp notes as [`Self::send_buffer_bytes`].
    pub recv_buffer_bytes: u64,
    /// Peer id identity for handshake + tracker announces.
    ///
    /// Azureus-style prefix (8 bytes typical) + 12 random bytes.
    /// Default: `seedchamp` → fixed `-sc0001-`. Or raw e.g. `-sc0001-`.
    /// Env: `SEEDCHAMP_PEER_ID_PREFIX` / `SEEDCHAMP_IDENTITY`.
    pub peer_id_prefix: String,
    /// HTTP User-Agent for tracker announces (and related HTTP).
    ///
    /// Default: `seedchamp/<pkg-version>`. Empty falls back to that default.
    /// Env: `SEEDCHAMP_HTTP_USER_AGENT`.
    pub http_user_agent: String,
    /// BEP 10 LTEP extended-handshake `v` (client version string).
    ///
    /// Empty (default) = derive from [`Self::peer_id_prefix`] via
    /// [`crate::library::resolve_ltep_client`]. Set explicitly to override
    /// (custom label). Env: `SEEDCHAMP_LTEP_CLIENT`.
    #[serde(default)]
    pub ltep_client: String,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0:6881".into(),
            encryption: "prefer-plain".into(),
            announce: true,
            send_buffer_bytes: 0,
            recv_buffer_bytes: 0,
            peer_id_prefix: "seedchamp".into(),
            http_user_agent: crate::tracker::tracker_user_agent().into(),
            ltep_client: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UploadSection {
    /// Seed upload I/O: `auto` | `pread` | `compio`.
    ///
    /// `auto`: Linux FS-gated Compio; Darwin pread; FreeBSD Compio.
    /// Parsed by [`crate::upload::UploadBackend`]. Env: `SEEDCHAMP_UPLOAD_BACKEND`.
    pub backend: String,
}

impl Default for UploadSection {
    fn default() -> Self {
        Self {
            backend: "auto".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SwarmConfig {
    /// Peer I/O worker threads; `0` = CPU count.
    pub peer_workers: u32,
    /// Piece-hash worker threads; `0` = CPU count.
    pub hash_workers: u32,
    /// Initial request pipeline depth (blocks) at connect / download reopen.
    /// Adaptive depth uses BDP from smoothed rate, not this as a permanent floor.
    /// Env: `SEEDCHAMP_PIPELINE`.
    pub pipeline: usize,
    /// Adaptive pipeline cap (blocks). Env: `SEEDCHAMP_PIPELINE_MAX`.
    pub pipeline_max: usize,
    /// Per-torrent leech piece-buffer budget (bytes). TOML: integer or `"256M"` / `"1G"`.
    /// `0` → default 256 MiB. Env: `SEEDCHAMP_STAGING_MEM_LIMIT`.
    #[serde(deserialize_with = "deserialize_byte_size_field", default)]
    pub staging_mem_limit: u64,
}

impl Default for SwarmConfig {
    fn default() -> Self {
        Self {
            peer_workers: 0,
            hash_workers: 0,
            pipeline: crate::runtime::pipeline::DEFAULT_PIPELINE,
            pipeline_max: crate::runtime::pipeline::MAX_PIPELINE,
            staging_mem_limit: crate::staging::DEFAULT_STAGING_MEM_LIMIT,
        }
    }
}

/// Durable leech piece writes after SHA-1 ([`crate::runtime::DiskWorker`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DiskConfig {
    /// Write backend: `auto` | `thread` | `uring` | `aio`.
    /// Env: `SEEDCHAMP_DISK_BACKEND`. Default `auto` (Linux io_uring / FreeBSD+macOS aio / thread).
    pub backend: String,
    /// Max in-flight piece write jobs (each holds a full piece buffer).
    /// Env: `SEEDCHAMP_DISK_DEPTH`. Default **32** (clamp 1–256).
    ///
    /// The worker also uses a `sync_channel(depth)` intake, so worst-case live
    /// piece buffers are about **2×depth** (channel full + inflight/waiting).
    /// Lower this on large piece lengths or memory-constrained hosts.
    pub depth: usize,
}

impl Default for DiskConfig {
    fn default() -> Self {
        Self {
            backend: "auto".into(),
            depth: crate::runtime::DEFAULT_DISK_DEPTH,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LimitsConfig {
    /// Max upload bytes/sec; 0 = unlimited. **Config is primary.**
    pub max_upload_bps: u64,
    /// Max download bytes/sec; 0 = unlimited.
    pub max_download_bps: u64,
    /// Soft floor of **useful** peers to chase while leeching (and while seeding
    /// only if [`Self::seed_dial_peers`]). Clamped to ≤ `max_peers`. Default 20.
    pub min_peers: u32,
    /// Max concurrent peers **per torrent** (inbound + outbound). Default 40.
    /// Enforced on dial and when an inbound handshake binds to a torrent.
    /// All connected peers may request pieces while leeching (no separate
    /// target_peers demotion); pipeline RTT naturally favors fast peers.
    pub max_peers: u32,
    /// When true, complete torrents may dial out / starve-announce to fill
    /// `min_peers`. Default **false** (seeders rely on inbound).
    pub seed_dial_peers: bool,
    /// Max live peer sessions process-wide (inbound + outbound). Default 2048.
    /// Inbound accepts are refused when at the cap (before handshake).
    pub max_connections: u32,
    /// Close seed↔seed (both complete, idle) after this many seconds (default 15).
    /// **0** = never close on this timer. General leech idle-close is not used
    /// (inbound useless peers tend to reconnect immediately).
    pub redundant_seed_idle_secs: u64,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_upload_bps: 0,
            max_download_bps: 0,
            min_peers: 20,
            max_peers: 40,
            seed_dial_peers: false,
            max_connections: 2048,
            redundant_seed_idle_secs: 15,
        }
    }
}

impl LimitsConfig {
    /// Clamp max ≥ 1 and min ≤ max.
    pub fn clamped_peer_limits(&self) -> (u32, u32) {
        let max_peers = self.max_peers.max(1);
        let min_peers = self.min_peers.min(max_peers);
        (min_peers, max_peers)
    }

    pub fn to_session_limits(&self) -> SessionLimits {
        let (min_peers, max_peers) = self.clamped_peer_limits();
        SessionLimits {
            max_upload_bps: self.max_upload_bps,
            max_download_bps: self.max_download_bps,
            min_peers,
            max_peers,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LoggingConfig {
    /// Log filter level: error | warn | info | debug | trace (reserved for tracing setup).
    pub level: String,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TrackerConfig {
    /// HTTP announce timeout (seconds).
    pub http_timeout_secs: u64,
    /// numwant on announce.
    pub numwant: u32,
    /// Minimum re-announce interval clamp (seconds).
    pub min_interval_secs: u32,
    /// Maximum re-announce interval clamp (seconds).
    pub max_interval_secs: u32,
    /// Max in-flight announces per tracker host (`host:port`). **0** = unlimited.
    /// Protects shared trackers when many torrents start at once.
    pub max_concurrent_per_host: u32,
    /// Delay between first announces when many torrents activate together (ms).
    /// Spreads startup load; 0 = schedule all immediately (still subject to
    /// per-host / global inflight caps).
    pub startup_stagger_ms: u64,
    /// Max concurrent announce jobs (any hosts). **0** = unlimited.
    pub max_inflight_announces: u32,
}

impl Default for TrackerConfig {
    fn default() -> Self {
        Self {
            http_timeout_secs: 30,
            numwant: 50,
            min_interval_secs: 60,
            max_interval_secs: 3600,
            // Conservative default: 2 parallel announces per tracker host.
            max_concurrent_per_host: 2,
            // 1000 torrents × 50ms ≈ 50s spread for first-wave announces.
            startup_stagger_ms: 50,
            max_inflight_announces: 16,
        }
    }
}

/// Global watch settings + list of watch directories.
///
/// Mirrors rtorrent `schedule2 = watch_*, interval, ((load.*, …))` with
/// optional `dl_path` templates (`{date}`, etc., local time).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WatchConfig {
    /// Master switch. When false, no watch thread runs.
    pub enabled: bool,
    /// How often to scan all dirs (seconds). rtorrent often uses 1.
    pub interval_secs: u64,
    /// Watch directories (empty = nothing to do even if enabled).
    pub dirs: Vec<WatchDirConfig>,
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            // 5s is plenty for drop-in torrents; 1s readdir was visible in truss at idle.
            interval_secs: 5,
            dirs: Vec::new(),
        }
    }
}

/// One rtorrent-style watch directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WatchDirConfig {
    /// Directory to scan for `.torrent` files.
    pub path: PathBuf,
    /// Optional label for logs/status (defaults to path basename).
    #[serde(default)]
    pub name: Option<String>,
    /// Download path template for torrent data (rtorrent `directory.*`).
    /// Empty / omitted → `paths.data_root`.
    ///
    /// Download path template. Placeholders (local time unless noted):
    /// `{date}` `{YYYY}` `{YY}` `{MM}` `{DD}` `{watch_name}` `{torrent_name}` `{ih8}`.
    /// `{torrent_name}` is sanitized; `{ih8}` is first 8 hex of infohash.
    /// Example: `"/dl/{watch_name}/{date}/{torrent_name}"`.
    #[serde(default)]
    pub dl_path: Option<String>,
    /// `load.start` vs `load.normal`: mark want_start and activate after add.
    pub start: bool,
    /// Remove the drop-in `.torrent` from the watch dir after successful import
    /// (rtorrent `d.delete_tied=`).
    pub delete_after_import: bool,
    /// Also remove when the torrent already exists in the catalog.
    pub delete_after_import_if_exists: bool,
    /// Save a catalog copy under `paths.torrent_dir` (independent of delete_after_import).
    pub save_torrent: bool,
    /// Per-dir enable (default true).
    pub enabled: bool,
}

impl Default for WatchDirConfig {
    fn default() -> Self {
        Self {
            path: PathBuf::new(),
            name: None,
            dl_path: None,
            start: false,
            delete_after_import: true,
            delete_after_import_if_exists: true,
            save_torrent: true,
            enabled: true,
        }
    }
}

// ─── XDG paths ───────────────────────────────────────────────────────────────

pub fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("USERPROFILE").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn xdg_config_home() -> PathBuf {
    env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".config"))
}

pub fn xdg_data_home() -> PathBuf {
    env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".local").join("share"))
}

pub fn default_config_path() -> PathBuf {
    xdg_config_home().join("seedchamp").join("config.toml")
}

pub fn default_data_dir() -> PathBuf {
    xdg_data_home().join("seedchamp")
}

/// Resolve config file path: explicit → `SEEDCHAMP_CONFIG` → XDG default.
pub fn resolve_config_path(explicit: Option<&Path>) -> PathBuf {
    if let Some(p) = explicit {
        return p.to_path_buf();
    }
    if let Some(p) = env::var_os("SEEDCHAMP_CONFIG") {
        return PathBuf::from(p);
    }
    default_config_path()
}

// ─── Load / save ─────────────────────────────────────────────────────────────

/// Load TOML from `path`. Missing file → defaults (not an error).
pub fn load_file(path: &Path) -> Result<Config> {
    if !path.is_file() {
        return Ok(Config::default());
    }
    let text =
        fs::read_to_string(path).map_err(|e| Error::Path(path.to_path_buf(), e.to_string()))?;
    let cfg: Config =
        toml::from_str(&text).map_err(|e| Error::Msg(format!("config {}: {e}", path.display())))?;
    Ok(cfg)
}

/// Load config from resolved path and apply environment overrides.
pub fn load(explicit: Option<&Path>) -> Result<(Config, PathBuf)> {
    let path = resolve_config_path(explicit);
    let mut cfg = load_file(&path)?;
    cfg.expand_paths();
    apply_env_overrides(&mut cfg);
    // Env paths may use ~/
    cfg.expand_paths();
    Ok((cfg, path))
}

/// Apply `SEEDCHAMP_*` environment overrides onto `cfg`.
pub fn apply_env_overrides(cfg: &mut Config) {
    if let Ok(v) = env::var("SEEDCHAMP_DB") {
        cfg.paths.db = PathBuf::from(v);
    }
    if let Ok(v) = env::var("SEEDCHAMP_DATA_ROOT") {
        cfg.paths.data_root = PathBuf::from(v);
    }
    if let Ok(v) = env::var("SEEDCHAMP_TORRENT_DIR") {
        cfg.paths.torrent_dir = PathBuf::from(v);
    }
    if let Ok(v) = env::var("SEEDCHAMP_LEECH_CACHE") {
        cfg.paths.leech_cache = PathBuf::from(v);
    }
    if let Ok(v) = env::var("SEEDCHAMP_LEECH_CACHE_SIZE") {
        if let Ok(n) = parse_byte_size(&v) {
            cfg.paths.leech_cache_size = n;
        }
    }
    if let Ok(v) = env::var("SEEDCHAMP_SOFT_DELETE_PURGE_DAYS") {
        if let Ok(n) = v.parse() {
            cfg.catalog.soft_delete_purge_days = n;
        }
    }
    if let Ok(v) = env::var("SEEDCHAMP_LISTEN") {
        cfg.network.listen = v;
    }
    if let Ok(v) = env::var("SEEDCHAMP_ENCRYPTION") {
        cfg.network.encryption = v;
    }
    if let Ok(v) = env::var("SEEDCHAMP_ANNOUNCE") {
        cfg.network.announce = parse_bool(&v).unwrap_or(cfg.network.announce);
    }
    if let Ok(v) = env::var("SEEDCHAMP_SEND_BUFFER_BYTES") {
        if let Ok(n) = parse_byte_size(&v) {
            cfg.network.send_buffer_bytes = n;
        }
    }
    if let Ok(v) = env::var("SEEDCHAMP_RECV_BUFFER_BYTES") {
        if let Ok(n) = parse_byte_size(&v) {
            cfg.network.recv_buffer_bytes = n;
        }
    }
    // Convenience: set both when only SOCKBUF is provided (harness-style).
    if let Ok(v) = env::var("SEEDCHAMP_SOCKBUF") {
        if let Ok(n) = parse_byte_size(&v) {
            cfg.network.send_buffer_bytes = n;
            cfg.network.recv_buffer_bytes = n;
        }
    }
    if let Ok(v) = env::var("SEEDCHAMP_PEER_ID_PREFIX") {
        cfg.network.peer_id_prefix = v;
    }
    // Alias: SEEDCHAMP_IDENTITY=seedchamp | sc | raw -xx0000-
    if let Ok(v) = env::var("SEEDCHAMP_IDENTITY") {
        cfg.network.peer_id_prefix = v;
    }
    if let Ok(v) = env::var("SEEDCHAMP_HTTP_USER_AGENT") {
        cfg.network.http_user_agent = v;
    }
    if let Ok(v) = env::var("SEEDCHAMP_LTEP_CLIENT") {
        cfg.network.ltep_client = v;
    }
    if let Ok(v) = env::var("SEEDCHAMP_UPLOAD_BACKEND") {
        cfg.upload.backend = v;
    }
    if let Ok(v) = env::var("SEEDCHAMP_PEER_WORKERS") {
        if let Ok(n) = v.parse() {
            cfg.swarm.peer_workers = n;
        }
    }
    if let Ok(v) = env::var("SEEDCHAMP_HASH_WORKERS") {
        if let Ok(n) = v.parse() {
            cfg.swarm.hash_workers = n;
        }
    }
    if let Ok(v) = env::var("SEEDCHAMP_PIPELINE") {
        if let Ok(n) = v.parse() {
            cfg.swarm.pipeline = n;
        }
    }
    if let Ok(v) = env::var("SEEDCHAMP_PIPELINE_MAX") {
        if let Ok(n) = v.parse::<usize>() {
            cfg.swarm.pipeline_max = n.max(1);
        }
    }
    if let Ok(v) = env::var("SEEDCHAMP_STAGING_MEM_LIMIT") {
        if let Ok(n) = parse_byte_size(&v) {
            cfg.swarm.staging_mem_limit = n;
        }
    }
    if let Ok(v) = env::var("SEEDCHAMP_DISK_BACKEND") {
        let t = v.trim();
        if !t.is_empty() {
            cfg.disk.backend = t.to_ascii_lowercase();
        }
    }
    if let Ok(v) = env::var("SEEDCHAMP_DISK_DEPTH") {
        if let Ok(n) = v.parse::<usize>() {
            cfg.disk.depth = n.clamp(1, 256);
        }
    }
    if let Ok(v) = env::var("SEEDCHAMP_MAX_UPLOAD_BPS") {
        if let Ok(n) = v.parse() {
            cfg.limits.max_upload_bps = n;
        }
    }
    if let Ok(v) = env::var("SEEDCHAMP_MAX_DOWNLOAD_BPS") {
        if let Ok(n) = v.parse() {
            cfg.limits.max_download_bps = n;
        }
    }
    if let Ok(v) = env::var("SEEDCHAMP_MAX_PEERS") {
        if let Ok(n) = v.parse() {
            cfg.limits.max_peers = n;
        }
    }
    if let Ok(v) = env::var("SEEDCHAMP_MAX_CONNECTIONS") {
        if let Ok(n) = v.parse() {
            cfg.limits.max_connections = n;
        }
    }
    if let Ok(v) = env::var("SEEDCHAMP_REDUNDANT_SEED_IDLE_SECS") {
        if let Ok(n) = v.parse() {
            cfg.limits.redundant_seed_idle_secs = n;
        }
    }
    if let Ok(v) = env::var("SEEDCHAMP_LOG") {
        cfg.logging.level = v;
    }
    if let Ok(v) = env::var("SEEDCHAMP_TRACKER_MAX_CONCURRENT_PER_HOST") {
        if let Ok(n) = v.parse() {
            cfg.tracker.max_concurrent_per_host = n;
        }
    }
    if let Ok(v) = env::var("SEEDCHAMP_TRACKER_STARTUP_STAGGER_MS") {
        if let Ok(n) = v.parse() {
            cfg.tracker.startup_stagger_ms = n;
        }
    }
    if let Ok(v) = env::var("SEEDCHAMP_TRACKER_MAX_INFLIGHT") {
        if let Ok(n) = v.parse() {
            cfg.tracker.max_inflight_announces = n;
        }
    }
    if let Ok(v) = env::var("SEEDCHAMP_WATCH") {
        cfg.watch.enabled = parse_bool(&v).unwrap_or(cfg.watch.enabled);
    }
    if let Ok(v) = env::var("SEEDCHAMP_WATCH_INTERVAL") {
        if let Ok(n) = v.parse() {
            cfg.watch.interval_secs = n;
        }
    }
}

fn parse_bool(s: &str) -> Option<bool> {
    match s.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Parse `4194304`, `4M`, `256K`, `1G` → bytes.
pub fn parse_byte_size(s: &str) -> std::result::Result<u64, String> {
    let s = s.trim().to_ascii_uppercase().replace(' ', "");
    if s.is_empty() {
        return Err("empty size".into());
    }
    let (num, mult) = if let Some(rest) = s.strip_suffix("GIB").or_else(|| s.strip_suffix('G')) {
        (rest, 1024u64 * 1024 * 1024)
    } else if let Some(rest) = s.strip_suffix("MIB").or_else(|| s.strip_suffix('M')) {
        (rest, 1024 * 1024)
    } else if let Some(rest) = s.strip_suffix("KIB").or_else(|| s.strip_suffix('K')) {
        (rest, 1024)
    } else if let Some(rest) = s.strip_suffix('B') {
        (rest, 1)
    } else {
        (s.as_str(), 1)
    };
    let n: u64 = num.parse().map_err(|_| format!("bad size {s:?}"))?;
    Ok(n.saturating_mul(mult))
}

/// Serde: TOML integer **or** string (`"256M"`, `"1G"`) → bytes.
fn deserialize_byte_size_field<'de, D>(deserializer: D) -> std::result::Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::{self, Visitor};
    use std::fmt;

    struct ByteSizeVisitor;
    impl<'de> Visitor<'de> for ByteSizeVisitor {
        type Value = u64;
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("byte size as integer or string like \"256M\" / \"1G\"")
        }
        fn visit_u64<E: de::Error>(self, v: u64) -> std::result::Result<u64, E> {
            Ok(v)
        }
        fn visit_i64<E: de::Error>(self, v: i64) -> std::result::Result<u64, E> {
            if v < 0 {
                return Err(E::custom("byte size must be non-negative"));
            }
            Ok(v as u64)
        }
        fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<u64, E> {
            parse_byte_size(v).map_err(E::custom)
        }
    }
    deserializer.deserialize_any(ByteSizeVisitor)
}

/// Write a commented template to `path` (creates parent dirs).
///
/// Body is the current [`Config::default()`] (respects XDG data home) plus a header.
pub fn write_template(path: &Path, force: bool) -> Result<()> {
    if path.is_file() && !force {
        return Err(Error::Msg(format!(
            "config already exists: {} (use --force to overwrite)",
            path.display()
        )));
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| Error::Path(parent.to_path_buf(), e.to_string()))?;
    }
    let mut body = String::from(TEMPLATE_HEADER);
    body.push_str(&to_toml_string(&Config::default())?);
    fs::write(path, body).map_err(|e| Error::Path(path.to_path_buf(), e.to_string()))?;
    Ok(())
}

/// Serialize effective config as TOML (for `config show`).
pub fn to_toml_string(cfg: &Config) -> Result<String> {
    toml::to_string_pretty(cfg).map_err(|e| Error::Msg(format!("serialize config: {e}")))
}

impl Config {
    pub fn encryption_mode(&self) -> Result<EncryptionMode> {
        self.network.encryption.parse().map_err(|e: Error| e)
    }

    pub fn listen_addr(&self) -> Result<SocketAddr> {
        self.network
            .listen
            .parse()
            .map_err(|e| Error::Msg(format!("bad network.listen {:?}: {e}", self.network.listen)))
    }

    /// Resolve `[upload].backend` (`auto` \| `pread` \| `compio`).
    pub fn upload_options(&self) -> Result<UploadOptions> {
        let backend = UploadBackend::parse(&self.upload.backend)?.resolve()?;
        Ok(UploadOptions { backend })
    }

    pub fn peer_workers_opt(&self) -> Option<usize> {
        if self.swarm.peer_workers == 0 {
            None
        } else {
            Some(self.swarm.peer_workers as usize)
        }
    }

    pub fn hash_workers_opt(&self) -> Option<usize> {
        if self.swarm.hash_workers == 0 {
            None
        } else {
            Some(self.swarm.hash_workers as usize)
        }
    }

    /// Push limit values into the catalog (config is primary for limits).
    pub fn apply_limits_to_catalog(&self, cat: &mut crate::catalog::Catalog) -> Result<()> {
        cat.set_session_limits(&self.limits.to_session_limits())
    }

    /// Startup catalog maintenance: limits, soft-delete purge, complete storage audit.
    ///
    /// Purge removes **catalog rows only** for torrents soft-deleted more than
    /// [`CatalogConfig::soft_delete_purge_days`] ago. Downloaded payload files
    /// are left on disk.
    ///
    /// Storage audit: for each `complete=1` torrent, `stat` every file; on
    /// missing / non-file / size ≠ expected mark incomplete, clear bitfield, stop.
    pub fn apply_startup_to_catalog(
        &self,
        cat: &mut crate::catalog::Catalog,
    ) -> Result<StartupCatalogReport> {
        self.apply_limits_to_catalog(cat)?;
        let days = self.catalog.soft_delete_purge_days;
        let purged = cat.purge_soft_deleted(days)?;
        if purged > 0 {
            tracing::info!(
                purged,
                days,
                "purged soft-deleted torrents from catalog (payload files kept)"
            );
        }
        let storage = cat.audit_complete_storage()?;
        if storage.demoted > 0 {
            tracing::warn!(
                checked = storage.checked,
                demoted = storage.demoted,
                "demoted complete torrents with missing/wrong-size payload files"
            );
        }
        Ok(StartupCatalogReport { purged, storage })
    }
}

/// Outcome of [`Config::apply_startup_to_catalog`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StartupCatalogReport {
    pub purged: usize,
    pub storage: crate::catalog::StorageAuditReport,
}

/// Header prepended by `config init` (body is serialized defaults).
pub const TEMPLATE_HEADER: &str = r#"# seedchamp configuration
# Location: $XDG_CONFIG_HOME/seedchamp/config.toml  (default ~/.config/seedchamp/config.toml)
# Data default: $XDG_DATA_HOME/seedchamp/            (default ~/.local/share/seedchamp/)
#
# Precedence: CLI flags > environment (SEEDCHAMP_*) > this file > built-in defaults.
# Rate limits in this file are primary (applied on process start).
#
# network.encryption: off | prefer-plain | prefer-rc4 | require-rc4
# network.send_buffer_bytes / recv_buffer_bytes:
#   0 (default) = keep kernel SO_SNDBUF/SO_RCVBUF — fine for seedboxes; no forced resize.
#   Positive = best-effort setsockopt size in bytes (env: SEEDCHAMP_SEND_BUFFER=4M, etc.).
#   Kernel may clamp/double; only raise if you know you need a larger TCP window.
# network.peer_id_prefix: seedchamp (default fixed -sc0001-) | sc | raw e.g. -sc0001-
#   env: SEEDCHAMP_PEER_ID_PREFIX / SEEDCHAMP_IDENTITY
# network.http_user_agent: tracker HTTP User-Agent (default seedchamp/<version>)
#   env: SEEDCHAMP_HTTP_USER_AGENT
# network.ltep_client: BEP 10 LTEP `v` (empty = derive from peer_id_prefix)
#   env: SEEDCHAMP_LTEP_CLIENT
# tui.default_sort: screen key/label (default "1")
# tui.screens: list sort screens (key, label, order[]) — o / 1 / 2 cycle
#   order tokens: off_first down_rate_desc up_rate_desc added_desc name_asc id_asc
# tui.theme: "default" | "soft" | path to a theme-only TOML under themes/
#   Colors live only in the theme file (not in this config). Stock themes:
#     themes/default.toml  — ANSI (current look)
#     themes/soft.toml     — truecolor soft 90s/modern terminal
#   Written by `seedchamp config init`. Partial theme files merge onto default.
# swarm.peer_workers: 0 = CPU count (seedchamp-io)
# swarm.hash_workers: 0 = CPU count (seedchamp-hash-*)
# swarm.pipeline: initial request depth at connect (default 16; env SEEDCHAMP_PIPELINE)
#   Adaptive depth = BDP (5s × smoothed rate / 16KiB), not a permanent floor.
# swarm.pipeline_max: adaptive pipeline cap (default 8192; env SEEDCHAMP_PIPELINE_MAX)
# swarm.staging_mem_limit: per-torrent leech piece-buffer budget (default 256M)
#   TOML: 268435456 or "256M" / "1G" (1024-based). Env: SEEDCHAMP_STAGING_MEM_LIMIT
#   Shared freelist; lazy-alloc up to N = limit / piece_length; freed when wanted complete.
# upload.backend: auto | pread | compio (default auto; env SEEDCHAMP_UPLOAD_BACKEND)
#   auto: Linux Compio on ext4/xfs/btrfs else pread; Darwin pread; FreeBSD Compio.
#   pread: blocking pread only. compio: force Compio on any FS (no FS gate).
#   SEEDCHAMP_UPLOAD_COMPIO_FS=all: Linux auto uses Compio on any filesystem.
# disk.backend: auto | thread | uring | aio (default auto; env SEEDCHAMP_DISK_BACKEND)
#   Linux: io_uring when available; FreeBSD/macOS: posix aio (writes); else sync pwrite thread.
#   TUI and CLI seed/leech both honor [disk] (file + env via apply_env_overrides).
# disk.depth: max in-flight piece write jobs (default 32; clamp 1–256; env SEEDCHAMP_DISK_DEPTH)
#   Each job holds a full piece buffer. Channel + inflight ≈ up to ~2×depth buffers RSS.
#   Lower on large pieces / low RAM. Hash workers block on full queue (intentional backpressure).
#   Linux io_uring ring size ≈ min(4×depth, 4096); a piece with more spans than ring entries fails.
# limits.min_peers: useful-peer floor to chase while leeching (default 20; ≤ max_peers)
# limits.max_peers: concurrent peers per torrent, inbound+outbound (default 40)
# limits.seed_dial_peers: if true, complete torrents dial out to fill min_peers (default false)
# limits.max_connections: process-wide peer sessions; inbound refused at cap (default 2048)
#   env: SEEDCHAMP_MAX_CONNECTIONS
# limits.redundant_seed_idle_secs: close seed↔seed idle peers (default 15; 0 = off)
#   env: SEEDCHAMP_REDUNDANT_SEED_IDLE_SECS
# catalog.soft_delete_purge_days: hard-remove soft-deleted catalog rows after N days
#   on startup (default 30; 0 = never). Never deletes downloaded payload files.
# tracker.max_concurrent_per_host: in-flight announces per host:port (0 = unlimited)
# tracker.startup_stagger_ms: delay between first announces when many start (0 = none)
# tracker.max_inflight_announces: global concurrent announce jobs (0 = unlimited)
# limits.max_upload_bps / max_download_bps: global wire caps (0 = unlimited, free path)
# logging.level: error | warn | info | debug | trace (reserved)
#
# [watch] — rtorrent schedule2 watch dirs
#   defaults: enabled=false, interval_secs=1, dirs=[]
#   per-dir defaults: start=false, delete_after_import=true,
#     delete_after_import_if_exists=true, save_torrent=true, enabled=true
#   dl_path omitted → paths.data_root
#   placeholders (local time): {date} {YYYY} {YY} {MM} {DD}
#     {watch_name} {torrent_name} (sanitized) {ih8} (infohash prefix)
#
# Example:
#   [watch]
#   enabled = true
#   interval_secs = 1
#
#   # Import only (load.normal)
#   [[watch.dirs]]
#   path = "~/watch"
#   dl_path = "~/downloads/{date}/{torrent_name}"
#
#   # Import and start (load.start)
#   [[watch.dirs]]
#   name = "start"
#   path = "~/watch/start"
#   dl_path = "~/downloads/{watch_name}/{date}/{torrent_name}"
#   start = true
#
# [tui]
# default_sort = "rate"   # 1 / rate  → ↓rate, ↑rate, added, name
# # default_sort = "name" # 2 / name  → name A–Z
# theme = "default"       # or "soft", or "themes/my.toml"
# Runtime: press o to cycle, or :sort rate|name
#
# paths.leech_cache: optional leech cache (empty = off). Wanted downloads that fit free
#   space (and leech_cache_size) stage under {leech_cache}/{infohash}/; on wanted-complete,
#   publish dest (hardlink/copy), swap catalog + live layout, delete stage.
#   Recommended on a fast local volume (typically SSD). Empty = off (use data_root).
#   Env: SEEDCHAMP_LEECH_CACHE
# paths.leech_cache_size: soft max committed bytes under the cache (0 = no soft cap).
#   Catalog reserved size of staged torrents; integer or "100G". Env: SEEDCHAMP_LEECH_CACHE_SIZE
# Env: SEEDCHAMP_SOCKBUF / SEEDCHAMP_SEND_BUFFER_BYTES / SEEDCHAMP_RECV_BUFFER_BYTES
#      SEEDCHAMP_TRACKER_MAX_CONCURRENT_PER_HOST / SEEDCHAMP_TRACKER_STARTUP_STAGGER_MS
#      SEEDCHAMP_TRACKER_MAX_INFLIGHT / SEEDCHAMP_WATCH / SEEDCHAMP_WATCH_INTERVAL

"#;

/// Expand a leading `~/` in path fields after load (optional helper for show/init).
pub fn expand_tilde(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix("~/") {
        return home_dir().join(rest);
    }
    if s == "~" {
        return home_dir();
    }
    p.to_path_buf()
}

impl Config {
    /// Expand `~/` in path fields (call after load for runtime use).
    pub fn expand_paths(&mut self) {
        self.paths.db = expand_tilde(&self.paths.db);
        self.paths.data_root = expand_tilde(&self.paths.data_root);
        if !self.paths.leech_cache.as_os_str().is_empty() {
            self.paths.leech_cache = expand_tilde(&self.paths.leech_cache);
        }
        self.paths.torrent_dir = expand_tilde(&self.paths.torrent_dir);
        for d in &mut self.watch.dirs {
            d.path = expand_tilde(&d.path);
            if let Some(ref p) = d.dl_path {
                d.dl_path = Some(expand_tilde(Path::new(p)).display().to_string());
            }
        }
    }

    /// Active watch dirs (master + per-dir enabled, non-empty path).
    pub fn active_watch_dirs(&self) -> Vec<&WatchDirConfig> {
        if !self.watch.enabled {
            return Vec::new();
        }
        self.watch
            .dirs
            .iter()
            .filter(|d| d.enabled && !d.path.as_os_str().is_empty())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_default_toml() {
        let c = Config::default();
        let s = to_toml_string(&c).unwrap();
        let c2: Config = toml::from_str(&s).unwrap();
        assert_eq!(c2.network.encryption, "prefer-plain");
        assert_eq!(c2.upload.backend, "auto");
        assert!(matches!(
            c2.upload_options().unwrap().backend,
            crate::upload::ResolvedUploadBackend::Auto
        ));
        assert_eq!(c2.limits.max_peers, 40);
        assert_eq!(c2.catalog.soft_delete_purge_days, 30);
    }

    #[test]
    fn init_body_roundtrips() {
        let mut body = String::from(TEMPLATE_HEADER);
        body.push_str(&to_toml_string(&Config::default()).unwrap());
        let c: Config = toml::from_str(&body).unwrap();
        assert!(c.network.announce);
        assert_eq!(c.tracker.numwant, 50);
        assert_eq!(c.network.encryption, "prefer-plain");
    }

    #[test]
    fn resolve_respects_env_config() {
        // Just ensure function doesn't panic
        let p = resolve_config_path(None);
        assert!(p.to_string_lossy().contains("seedchamp"));
    }

    #[test]
    fn parse_byte_size_units() {
        assert_eq!(parse_byte_size("4M").unwrap(), 4 * 1024 * 1024);
        assert_eq!(parse_byte_size("256K").unwrap(), 256 * 1024);
        assert_eq!(parse_byte_size("4194304").unwrap(), 4194304);
        assert_eq!(parse_byte_size("0").unwrap(), 0);
        assert_eq!(parse_byte_size("1G").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_byte_size("256MiB").unwrap(), 256 * 1024 * 1024);
    }

    #[test]
    fn staging_mem_limit_toml_string_and_int() {
        let s = r#"
            [swarm]
            staging_mem_limit = "256M"
        "#;
        let c: Config = toml::from_str(s).unwrap();
        assert_eq!(c.swarm.staging_mem_limit, 256 * 1024 * 1024);
        let s2 = r#"
            [swarm]
            staging_mem_limit = 1048576
        "#;
        let c2: Config = toml::from_str(s2).unwrap();
        assert_eq!(c2.swarm.staging_mem_limit, 1048576);
    }

    #[test]
    fn network_buffer_defaults() {
        let c = Config::default();
        assert_eq!(c.network.send_buffer_bytes, 0);
        assert_eq!(c.network.recv_buffer_bytes, 0);
        let mut c = Config::default();
        c.network.send_buffer_bytes = 4 * 1024 * 1024;
        c.network.recv_buffer_bytes = 2 * 1024 * 1024;
        assert_eq!(c.network.send_buffer_bytes, 4 * 1024 * 1024);
        assert_eq!(c.network.recv_buffer_bytes, 2 * 1024 * 1024);
    }

    #[test]
    fn tracker_announce_limit_defaults() {
        let c = Config::default();
        assert_eq!(c.tracker.max_concurrent_per_host, 2);
        assert_eq!(c.tracker.startup_stagger_ms, 50);
        assert_eq!(c.tracker.max_inflight_announces, 16);
    }

    #[test]
    fn max_peers_and_connections_defaults() {
        let c = Config::default();
        assert_eq!(c.limits.max_peers, 40);
        assert_eq!(c.limits.max_connections, 2048);
        assert_eq!(c.limits.redundant_seed_idle_secs, 15);
    }

    #[test]
    fn peer_id_prefix_and_ua_defaults() {
        let c = Config::default();
        assert_eq!(c.network.peer_id_prefix, "seedchamp");
        assert_eq!(
            c.network.http_user_agent,
            crate::tracker::tracker_user_agent()
        );
        assert!(c.network.ltep_client.is_empty());
    }

    #[test]
    fn watch_dirs_roundtrip() {
        let toml = r#"
[watch]
enabled = true
interval_secs = 1

[[watch.dirs]]
path = "~/watch"
dl_path = "~/downloads/{date}/{torrent_name}"

[[watch.dirs]]
name = "start"
path = "~/watch/start"
dl_path = "~/downloads/{watch_name}/{date}/{torrent_name}"
start = true
"#;
        let c: Config = toml::from_str(toml).unwrap();
        assert!(c.watch.enabled);
        assert_eq!(c.watch.dirs.len(), 2);
        assert!(!c.watch.dirs[0].start);
        assert!(c.watch.dirs[1].start);
        assert_eq!(c.watch.dirs[1].name.as_deref(), Some("start"));
        assert_eq!(
            c.watch.dirs[0].dl_path.as_deref(),
            Some("~/downloads/{date}/{torrent_name}")
        );
        assert!(c.watch.dirs[0].save_torrent);
        assert!(c.watch.dirs[0].delete_after_import);
    }
}
