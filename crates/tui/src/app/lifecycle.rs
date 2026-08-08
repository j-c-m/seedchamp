//! Construction, control plane, watcher, shutdown.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use ratatui::layout::Rect;
use ratatui::widgets::TableState;

use seedchamp_engine::{
    init_activity_logging, spawn_control_plane, spawn_watcher, Catalog, ControlHandle,
    ProcessSample, ProcessSampleState, Result, RuntimeConfig, SessionLimits, SessionSnapshot,
    WatchConfig, VERSION,
};

use crate::theme::Theme;

use super::{ListSort, Mode};

impl super::App {
    pub fn new_with_runtime_and_watch_sort(
        db: &Path,
        runtime: RuntimeConfig,
        limits: SessionLimits,
        watch: WatchConfig,
        data_root: PathBuf,
        torrent_dir: PathBuf,
        leech_cache: PathBuf,
        leech_cache_size: u64,
        list_sort: ListSort,
        theme: Theme,
    ) -> Result<Self> {
        // Capture engine tracing into the TUI log ring (info by default; RUST_LOG overrides).
        let activity = init_activity_logging("info", 2_000);
        let mut app = Self {
            db: db.to_path_buf(),
            theme,
            catalog: Catalog::open_for_ui(db).ok(),
            rows: Vec::new(),
            selected: None,
            selected_torrent_id: None,
            list_table_state: TableState::default(),
            body_area: Rect::default(),
            mode: Mode::List,
            input: String::new(),
            filter: String::new(),
            status: String::new(),
            detail: None,
            detail_scroll: 0,
            detail_view_h: 20,
            detail_content_lines: 0,
            pane_scroll: 0,
            pane_view_h: 20,
            pane_content_lines: 0,
            files_torrent_id: None,
            files: Vec::new(),
            file_tree: Vec::new(),
            file_collapsed: HashSet::new(),
            file_selected: 0,
            file_table_state: TableState::default(),
            peer_selected_id: None,
            peer_scroll: 0,
            activity,
            log_lines: Vec::new(),
            log_from_end: 0,
            log_anchor_seq: None,
            log_view_h: 20,
            log_view_w: 80,
            last_log_seq: 0,
            log_filter: String::new(),
            relocate_torrent_id: None,
            path_completions: Vec::new(),
            path_completion_idx: 0,
            path_completion_base: String::new(),
            limits,
            engine_version: VERSION,
            control: None,
            _plane: None,
            snap: SessionSnapshot::default(),
            listen: runtime.listen,
            encryption: runtime.encryption,
            peer_workers: 0,
            runtime_template: runtime,
            data_root,
            torrent_dir,
            leech_cache,
            leech_cache_size,
            watch_cfg: watch,
            _watch: None,
            row_ui: HashMap::new(),
            pending_gone: HashSet::new(),
            list_sort,
            last_refresh: Instant::now() - Duration::from_secs(10),
            last_sql: Instant::now() - Duration::from_secs(10),
            last_rate_sort: Instant::now() - Duration::from_secs(10),
            last_status_key: String::new(),
            last_snap_key: String::new(),
            quitting: false,
            last_event: String::new(),
            last_event_at: Instant::now() - Duration::from_secs(60),
            last_error: String::new(),
            sticky_status: None,
            bg_tx: {
                // placeholder; replaced immediately below
                let (tx, _rx) = mpsc::channel();
                tx
            },
            bg_rx: {
                let (_tx, rx) = mpsc::channel();
                rx
            },
            process_sample: ProcessSample::default(),
            filesystems: Vec::new(),
            process_sample_state: ProcessSampleState::new(),
            // Force first Status open to sample immediately.
            last_process_sample: Instant::now() - Duration::from_secs(10),
        };
        let (bg_tx, bg_rx) = mpsc::channel();
        app.bg_tx = bg_tx;
        app.bg_rx = bg_rx;
        // Config-primary limits: ensure catalog mirrors what the process is using.
        if let Some(cat) = app.catalog.as_mut() {
            let _ = cat.set_session_limits(&app.limits);
        }
        app.start_control_plane()?;
        app.start_watcher();
        // First list on catalog reader; wait briefly so the first paint is not empty.
        app.kick_catalog_list();
        let deadline = Instant::now() + Duration::from_secs(2);
        while app.rows.is_empty() && Instant::now() < deadline {
            let (_any, list_ok) = app.poll_control_events();
            if list_ok || !app.rows.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        app.refresh()?;
        Ok(app)
    }

    /// Reuse open catalog; reopen only if missing/failed.
    ///
    /// Uses a short SQLite busy timeout so list/detail refresh never freezes the
    /// TUI behind engine bitfield commits during fast download.
    pub(super) fn catalog(&mut self) -> Result<&mut Catalog> {
        if self.catalog.is_none() {
            self.catalog = Some(Catalog::open_for_ui(&self.db)?);
        }
        self.catalog
            .as_mut()
            .ok_or_else(|| seedchamp_engine::Error::Msg("catalog not open".into()))
    }

    pub(super) fn start_watcher(&mut self) {
        if !self.watch_cfg.enabled || self.watch_cfg.dirs.is_empty() {
            return;
        }
        let n = self
            .watch_cfg
            .dirs
            .iter()
            .filter(|d| d.enabled && !d.path.as_os_str().is_empty())
            .count();
        if n == 0 {
            return;
        }
        let control = self.control.clone();
        let bg = self.bg_tx.clone();
        let on_load: seedchamp_engine::WatchCallback = std::sync::Arc::new(move |ev| {
            let id = ev.report.id;
            let name = ev.report.name.clone();
            let label = ev.watch_name.clone();
            let msg = if ev.report.already_existed {
                format!("watch[{label}]: exists id={id} {name}")
            } else {
                format!("watch[{label}]: added id={id} {name}")
            };
            let _ = bg.send(msg);
            if ev.start {
                if let Some(ref c) = control {
                    let _ = c.request_start(id);
                }
            }
        });
        match spawn_watcher(
            self.db.clone(),
            self.watch_cfg.clone(),
            self.data_root.clone(),
            self.torrent_dir.clone(),
            self.leech_cache.clone(),
            self.leech_cache_size,
            Some(on_load),
        ) {
            Ok(h) => {
                self.status = format!(
                    "watch: {n} dir(s) · every {}s",
                    self.watch_cfg.interval_secs.max(1)
                );
                self._watch = Some(h);
            }
            Err(e) => {
                self.last_event = format!("watch: {e}");
                self.last_event_at = Instant::now();
            }
        }
    }

    /// Start the control plane. Interactive TUI requires this — failure aborts startup.
    pub fn start_control_plane(&mut self) -> Result<()> {
        if self.control.is_some() {
            return Ok(());
        }
        if let Some(parent) = self.db.parent() {
            if !parent.as_os_str().is_empty() {
                let _ = std::fs::create_dir_all(parent);
            }
        }
        let _ = Catalog::open(&self.db)?;
        let cfg = RuntimeConfig {
            listen: self.listen,
            encryption: self.encryption,
            ..self.runtime_template.clone()
        };
        let (handle, plane) = spawn_control_plane(&self.db, cfg)
            .map_err(|e| seedchamp_engine::Error::Msg(format!("control plane failed: {e}")))?;
        if let Ok(info) = handle.runtime_info() {
            self.peer_workers = info.peer_workers;
            self.status = format!(
                "control · {} peer I/O workers · {}",
                info.peer_workers, info.listen
            );
        } else {
            self.status = format!("control up · listen {}", self.listen);
        }
        self.control = Some(handle);
        self._plane = Some(plane);
        Ok(())
    }

    /// Live mutations require control. On miss: status note, no catalog write.
    pub(super) fn require_control(&self) -> Option<&ControlHandle> {
        self.control.as_ref()
    }

    /// Start quit-time stopped announces without blocking the TUI thread.
    ///
    /// Call [`Self::poll_shutdown`] + redraw until it returns true, then
    /// [`Self::finish_shutdown`].
    pub fn begin_shutdown(&mut self) {
        if self.quitting {
            return;
        }
        self.quitting = true;
        // Do not let a sticky event obscure engine quit progress.
        self.last_event.clear();
        self.status = "quitting…".into();
        if let Some(c) = &self.control {
            c.request_shutdown();
        }
    }

    /// Pull live quit status into `self.status`. Returns `true` when the
    /// control session is gone (stopped announces finished / no session).
    pub fn poll_shutdown(&mut self) -> bool {
        let Some(c) = &self.control else {
            self.status = "quit — done".into();
            return true;
        };
        if let Ok(snap) = c.snapshot() {
            if snap.lock_busy {
                if !snap.status_line.is_empty() {
                    self.status = snap.status_line;
                }
            } else {
                if !snap.status_line.is_empty() {
                    self.status = snap.status_line.clone();
                } else if self.status.is_empty() {
                    self.status = "quitting…".into();
                }
                self.snap = snap;
            }
        }
        if !c.session_alive() {
            if !self.status.contains("done") && !self.status.contains("stopped") {
                self.status = "quit — done".into();
            }
            return true;
        }
        false
    }

    /// Drop control handle and join the control plane thread.
    pub fn finish_shutdown(&mut self) {
        let _ = self.control.take();
        // Join so process exit does not kill mid quit-time stopped announces.
        drop(self._plane.take());
    }
}
