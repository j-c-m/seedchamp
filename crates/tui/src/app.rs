//! TUI application state — talks only to the control plane (never peer I/O).
//!
//! **Never blocks:** keys only `send` commands; each frame drains `ControlEvent`s
//! and takes a non-blocking snapshot. Full catalog list SQL runs on the control
//! **catalog reader** (`seedchamp-cread`). Add-from-URL runs on a background thread.

mod commands;
mod events;
mod files;
mod lifecycle;
mod list;
mod log;
mod peers;

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use ratatui::layout::Rect;
use ratatui::widgets::TableState;

use seedchamp_engine::{
    ActivityLog, Catalog, ControlHandle, ControlPlane, EncryptionMode, FileProgress,
    FilesystemUsage, LogLine, ProcessSample, ProcessSampleState, Result, RuntimeConfig,
    SessionLimits, SessionSnapshot, TorrentDetail, TorrentListRow, WatchConfig, WatchHandle,
};

use crate::file_tree::FileTreeRow;

pub use crate::sort::{ListSort, ListSortScreen, SortCriterion};
use crate::theme::Theme;

fn level_label(level: char) -> &'static str {
    match level {
        'E' => "err",
        'W' => "wrn",
        'I' => "inf",
        'D' => "dbg",
        'T' => "trc",
        _ => "?",
    }
}

/// Lower = more severe (for `>=w` filters).
fn level_rank(level: char) -> u8 {
    match level {
        'E' => 0,
        'W' => 1,
        'I' => 2,
        'D' => 3,
        'T' => 4,
        _ => 9,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    List,
    Detail,
    Peers,
    /// Per-file download on/off for the selected torrent.
    Files,
    /// Live activity log (tracing + engine events).
    Log,
    /// Help text for the log screen only (returns to Log).
    LogHelp,
    /// Substring filter prompt for the log screen (returns to Log on Enter/Esc).
    LogFilter,
    Filter,
    Palette,
    /// Prompt for new data_root (Ctrl-O); uses `input` + `relocate_torrent_id`.
    Relocate,
    Help,
    /// Process + engine activity (seedchamp “top”). Sample only while open.
    Status,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaletteAction {
    None,
    Quit,
}

/// One sort key in a config-driven screen `order` list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowUiStatus {
    /// Hash recheck in progress (`good` drives % column toward final have).
    Rechecking { good: u32, piece_count: u32 },
    /// Brief flash after a successful recheck.
    RecheckDone { complete: bool, at: Instant },
    /// Brief flash after recheck error.
    RecheckFailed { at: Instant },
}

impl RowUiStatus {
    /// How long result flashes stay visible in the RUN column.
    pub const FLASH_SECS: u64 = 8;

    pub fn is_expired(&self) -> bool {
        match self {
            RowUiStatus::Rechecking { .. } => false,
            RowUiStatus::RecheckDone { at, .. } | RowUiStatus::RecheckFailed { at } => {
                at.elapsed() >= Duration::from_secs(Self::FLASH_SECS)
            }
        }
    }

    /// Short label for the RUN column (≤3–4 chars to fit width 3–4).
    pub fn run_label(&self) -> &'static str {
        match self {
            RowUiStatus::Rechecking { .. } => "chk",
            RowUiStatus::RecheckDone { complete: true, .. } => "ok",
            RowUiStatus::RecheckDone {
                complete: false, ..
            } => "bad",
            RowUiStatus::RecheckFailed { .. } => "err",
        }
    }
}

pub struct App {
    pub db: PathBuf,
    /// Semantic color theme (from `tui.theme` / theme file).
    pub theme: Theme,
    /// Long-lived catalog (169 MiB DBs must not be reopened every refresh).
    catalog: Option<Catalog>,
    pub rows: Vec<TorrentListRow>,
    /// List cursor; **`None`** = nothing selected (startup default — top of list visible).
    pub selected: Option<usize>,
    /// Stable selection across SQL reloads and re-sorts (torrent id).
    selected_torrent_id: Option<i64>,
    /// List table scroll offset + selection (keeps highlight on-screen for large catalogs).
    pub list_table_state: TableState,
    /// Last frame body panel rect (for mouse hit-testing: list click, etc.).
    pub body_area: Rect,
    pub mode: Mode,
    pub input: String,
    pub filter: String,
    pub status: String,
    pub detail: Option<TorrentDetail>,
    pub detail_scroll: u16,
    /// Last detail panel content height (inner rows) for scroll clamp / page size.
    pub detail_view_h: u16,
    /// Last detail content line count (pre-wrap) for scroll clamp.
    pub detail_content_lines: u16,
    /// Shared vertical scroll for Status / Help / Log help (text panels).
    pub pane_scroll: u16,
    /// Last pane viewport height (inner rows) for page / clamp.
    pub pane_view_h: u16,
    /// Last pane content line count (pre-wrap) for clamp.
    pub pane_content_lines: u16,
    /// Files screen: torrent id + rows + selection.
    pub files_torrent_id: Option<i64>,
    /// Flat file list (catalog source of truth).
    pub files: Vec<FileProgress>,
    /// Visible tree rows (dirs + files) for the files screen.
    pub file_tree: Vec<FileTreeRow>,
    /// Collapsed directory prefixes (`foo/bar`). Empty = all expanded.
    file_collapsed: HashSet<String>,
    /// Cursor into `file_tree` (not into flat `files`).
    pub file_selected: usize,
    pub file_table_state: TableState,
    /// Peers screen: stable cursor by engine peer id (survives rate re-sort).
    pub peer_selected_id: Option<u64>,
    /// Peers table scroll offset (absolute row into the sorted peer list).
    pub peer_scroll: usize,
    /// Ring buffer of tracing / engine activity (TUI log screen).
    pub activity: std::sync::Arc<ActivityLog>,
    /// Cached log lines for draw (refreshed on tick / open).
    pub log_lines: Vec<LogLine>,
    /// Filtered entries after the viewport end (`0` = follow newest / live tail).
    pub log_from_end: usize,
    /// When scrolled back, pin the **top** visible line by `LogLine.seq` so
    /// appends / ring rotation do not slide content under the cursor.
    /// `None` while following the live tail.
    log_anchor_seq: Option<u64>,
    /// Last log panel content size (set by `draw_log`) for page/Home/clamp.
    pub log_view_h: usize,
    pub log_view_w: usize,
    pub last_log_seq: u64,
    /// Case-insensitive substring filter over log lines (display only; ring keeps all).
    pub log_filter: String,
    /// Target for Mode::Relocate.
    pub relocate_torrent_id: Option<i64>,
    /// Path tab-completion candidates (Relocate mode).
    path_completions: Vec<String>,
    path_completion_idx: usize,
    /// Input string when `path_completions` was last built (invalidate on edit).
    path_completion_base: String,
    pub limits: SessionLimits,
    pub engine_version: &'static str,
    /// Control plane handle only — no direct SessionRuntime / sockets.
    pub control: Option<ControlHandle>,
    /// Keeps control thread alive for process lifetime of the TUI.
    _plane: Option<ControlPlane>,
    pub snap: SessionSnapshot,
    pub listen: SocketAddr,
    pub encryption: EncryptionMode,
    pub peer_workers: usize,
    /// From config: upload backend, announce, pipeline, workers.
    pub runtime_template: RuntimeConfig,
    /// Default data root for watch dirs that omit `dl_path`.
    pub data_root: PathBuf,
    /// Where optional save_torrent copies go.
    pub torrent_dir: PathBuf,
    /// Optional SSD leech cache volume (`paths.leech_cache`).
    pub leech_cache: PathBuf,
    /// Soft max committed bytes under leech_cache (`0` = no soft cap).
    pub leech_cache_size: u64,
    watch_cfg: WatchConfig,
    /// Directory watcher (rtorrent schedule2 watch_*).
    _watch: Option<WatchHandle>,
    /// Per-torrent transient UI status (recheck progress / result).
    pub row_ui: HashMap<i64, RowUiStatus>,
    /// Ids dropped from the list optimistically (Ctrl-D / :remove). Strip from
    /// CatalogList until SoftDeleted/Removed or *Failed so stale SQL cannot
    /// resurrect a row still present in SQLite.
    pending_gone: HashSet<i64>,
    /// List sort mode (from config, changeable with `o` / `:sort`).
    pub list_sort: ListSort,
    last_refresh: Instant,
    /// Wall time of last **applied** full catalog list (CatalogList event).
    last_sql: Instant,
    /// Throttle full list re-sorts driven by live rates (700-row catalogs).
    last_rate_sort: Instant,
    /// Last status summary used to decide if a redraw is needed.
    last_status_key: String,
    /// Fingerprint of live snap rates/progress so 1 Hz redraws still fire.
    last_snap_key: String,
    /// Ctrl-q / `:quit` in progress — show engine quit status, ignore input.
    pub quitting: bool,
    /// Last control/bg event (shown on yellow status for a few seconds).
    last_event: String,
    last_event_at: Instant,
    /// Sticky error until a non-error success event replaces it (or new error).
    last_error: String,
    /// Critical sticky status (disk worker permanently dead): overrides all
    /// other bar text for the life of the process — operator must restart.
    sticky_status: Option<String>,
    /// Background job status (add-from-URL, etc.).
    bg_tx: Sender<String>,
    bg_rx: Receiver<String>,
    /// Last OS process sample for Mode::Status (TUI-thread only; no engine locks).
    pub process_sample: ProcessSample,
    /// Filesystems for open torrents + default download root (Status only).
    pub filesystems: Vec<FilesystemUsage>,
    /// Delta state for CPU % / I/O rates (Status only).
    process_sample_state: ProcessSampleState,
    /// Wall time of last process sample (throttle ≤1 Hz while Status open).
    last_process_sample: Instant,
}

impl Drop for App {
    fn drop(&mut self) {
        // Ensure control joins even if the quit UI path was skipped (panic, etc.).
        if self.control.is_none() && self._plane.is_none() {
            return;
        }
        self.begin_shutdown();
        while !self.poll_shutdown() {
            thread::sleep(Duration::from_millis(50));
        }
        self.finish_shutdown();
    }
}

/// Apply signed delta to a vertical scroll offset, clamped to `[0, max]`.
fn scroll_apply(cur: u16, delta: i16, max: u16) -> u16 {
    if delta < 0 {
        cur.saturating_sub((-delta) as u16).min(max)
    } else {
        cur.saturating_add(delta as u16).min(max)
    }
}

/// Typed prefix used for common-prefix comparison (expanded form).
fn parse_id_or_selected(arg: Option<&str>, app: &App) -> Result<Option<i64>> {
    match arg {
        Some(s) => {
            let cat = Catalog::open(&app.db)?;
            Ok(Some(cat.resolve_torrent_ref(s)?))
        }
        None => Ok(app.selected_id()),
    }
}
