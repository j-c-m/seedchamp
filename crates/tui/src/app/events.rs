//! Control events, snapshot refresh, status line.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use seedchamp_engine::{ControlEvent, Result};

use crate::helpers::{is_disk_worker_dead_status, status_msg_clears_error, status_msg_is_error};

use super::{Mode, RowUiStatus};

impl super::App {
    pub fn tick_refresh(&mut self) -> Result<bool> {
        let mut dirty = false;
        // Always drain control replies first (non-blocking).
        if self.poll_control_events().0 {
            dirty = true;
        }
        if self.poll_bg_status() {
            dirty = true;
        }
        self.expire_row_ui();
        // Snapshot every 1s (matches event poll max wait; keys still wake immediately).
        const SNAPSHOT_INTERVAL: Duration = Duration::from_secs(1);
        if self.last_refresh.elapsed() >= SNAPSHOT_INTERVAL {
            if self.refresh()? {
                dirty = true;
            }
            // Keep per-file % current while on the files screen.
            if self.mode == Mode::Files {
                let _ = self.refresh_files();
                dirty = true;
            }
        }
        // Process metrics only while Status is open (≤1 Hz, TUI thread, no engine locks).
        // Independent of snapshot path so closed Status ⇒ zero sampling cost.
        if self.mode == Mode::Status && self.maybe_sample_process() {
            dirty = true;
        }
        Ok(dirty)
    }
    pub(super) fn expire_row_ui(&mut self) {
        self.row_ui.retain(|_, st| !st.is_expired());
    }

    pub(super) fn mark_rechecking(&mut self, id: i64) {
        let piece_count = self
            .rows
            .iter()
            .find(|r| r.id == id)
            .map(|r| r.piece_count)
            .unwrap_or(0);
        self.row_ui.insert(
            id,
            RowUiStatus::Rechecking {
                good: 0,
                piece_count,
            },
        );
        if let Some(r) = self.rows.iter_mut().find(|r| r.id == id) {
            r.have_count = 0;
            r.complete = false;
            r.state = "checking".into();
        }
    }

    pub(super) fn poll_bg_status(&mut self) -> bool {
        let mut any = false;
        while let Ok(msg) = self.bg_rx.try_recv() {
            self.note_event(msg);
            any = true;
        }
        if any {
            self.request_catalog_list();
        }
        any
    }

    /// Queue full-list SQL on the catalog reader (never blocks).
    pub(super) fn kick_catalog_list(&self) {
        if let Some(c) = &self.control {
            let _ = c.request_list_catalog(self.filter.clone());
        }
    }

    /// Force a list refresh soon (filter change, control mutations, …).
    pub(super) fn request_catalog_list(&mut self) {
        self.last_sql = Instant::now() - Duration::from_secs(10);
        self.kick_catalog_list();
    }

    /// Apply a successful CatalogList event (filter must still match).
    pub(super) fn apply_catalog_list(
        &mut self,
        filter: String,
        mut rows: Vec<seedchamp_engine::TorrentListRow>,
        limits: seedchamp_engine::SessionLimits,
    ) -> bool {
        if filter != self.filter {
            self.kick_catalog_list();
            return false;
        }
        // Optimistic Ctrl-D / :remove: hide until SoftDeleted/Removed or *Failed.
        if !self.pending_gone.is_empty() {
            rows.retain(|r| !self.pending_gone.contains(&r.id));
        }
        let keep = self.selected_torrent_id;
        self.rows = rows;
        self.selected_torrent_id = keep;
        self.apply_list_sort();
        self.restore_selection();
        self.limits = limits;
        self.last_rate_sort = Instant::now();
        self.last_sql = Instant::now();
        true
    }

    pub(super) fn note_event(&mut self, msg: String) {
        if status_msg_is_error(&msg) {
            self.last_error = msg.clone();
        } else if status_msg_clears_error(&msg) {
            self.last_error.clear();
        }
        self.last_event = msg;
        self.last_event_at = Instant::now();
    }

    /// Apply async control-plane replies. Never waits.
    /// Returns `(any_ui_change, catalog_list_applied)`.
    pub fn poll_control_events(&mut self) -> (bool, bool) {
        let Some(c) = &self.control else {
            return (false, false);
        };
        let mut any = false;
        let mut list_applied = false;
        for ev in c.drain_events() {
            any = true;
            match ev {
                ControlEvent::Ready {
                    listen,
                    peer_workers,
                } => {
                    self.peer_workers = peer_workers;
                    self.note_event(format!("control ready · {peer_workers} io · {listen}"));
                }
                ControlEvent::Status(s) => {
                    self.note_event(s);
                }
                ControlEvent::LimitsUpdated { limits } => {
                    self.limits = limits;
                    self.note_event("limits updated".into());
                }
                ControlEvent::LimitsFailed { error } => {
                    self.note_event(format!("limits failed: {error}"));
                }
                ControlEvent::CatalogList {
                    filter,
                    rows,
                    limits,
                } => {
                    if self.apply_catalog_list(filter, rows, limits) {
                        list_applied = true;
                    }
                }
                ControlEvent::CatalogListFailed { filter, error } => {
                    if filter == self.filter {
                        if self.rows.is_empty() {
                            self.note_event(format!("catalog busy: {error}"));
                        }
                        self.last_sql = Instant::now() - Duration::from_millis(1500);
                    } else {
                        self.kick_catalog_list();
                    }
                }
                ControlEvent::Started { id } => {
                    if let Some(r) = self.rows.iter_mut().find(|r| r.id == id) {
                        r.want_start = true;
                    }
                    self.note_event(format!("#{id} started"));
                    self.request_catalog_list();
                }
                ControlEvent::StartFailed { id, error } => {
                    if let Some(r) = self.rows.iter_mut().find(|r| r.id == id) {
                        r.want_start = false;
                    }
                    self.note_event(format!("#{id} start failed: {error}"));
                }
                ControlEvent::Stopped { id } => {
                    if let Some(r) = self.rows.iter_mut().find(|r| r.id == id) {
                        r.want_start = false;
                    }
                    self.note_event(format!("#{id} stopped"));
                    self.request_catalog_list();
                }
                ControlEvent::StopFailed { id, error } => {
                    self.note_event(format!("#{id} stop failed: {error}"));
                }
                ControlEvent::RecheckProgress {
                    id,
                    piece_count,
                    checked,
                    good,
                    bad: _,
                    missing: _,
                } => {
                    let _ = checked;
                    self.row_ui
                        .insert(id, RowUiStatus::Rechecking { good, piece_count });
                    if let Some(r) = self.rows.iter_mut().find(|r| r.id == id) {
                        r.have_count = good;
                        r.piece_count = piece_count;
                        r.complete = false;
                        r.state = "checking".into();
                    }
                }
                ControlEvent::Rechecked {
                    id,
                    message,
                    complete,
                    good,
                    bad,
                    missing,
                    piece_count,
                } => {
                    self.row_ui.insert(
                        id,
                        RowUiStatus::RecheckDone {
                            complete,
                            at: Instant::now(),
                        },
                    );
                    if let Some(r) = self.rows.iter_mut().find(|r| r.id == id) {
                        r.complete = complete;
                        r.have_count = good;
                        if piece_count > 0 {
                            r.piece_count = piece_count;
                        }
                        r.state = if complete {
                            "complete".into()
                        } else {
                            "incomplete".into()
                        };
                    }
                    self.note_event(if message.is_empty() {
                        format!("#{id} recheck good={good} bad={bad} miss={missing}")
                    } else {
                        message
                    });
                    self.request_catalog_list();
                }
                ControlEvent::RecheckFailed { id, error } => {
                    self.row_ui
                        .insert(id, RowUiStatus::RecheckFailed { at: Instant::now() });
                    self.note_event(format!("#{id} recheck failed: {error}"));
                }
                ControlEvent::Relocated {
                    id,
                    data_root,
                    note,
                } => {
                    if let Some(r) = self.rows.iter_mut().find(|r| r.id == id) {
                        r.data_root = Some(data_root.display().to_string());
                    }
                    self.note_event(format!("#{id} {note}"));
                    self.request_catalog_list();
                }
                ControlEvent::RelocateFailed { id, error } => {
                    self.note_event(format!("#{id} relocate failed: {error}"));
                }
                ControlEvent::SoftDeleted { id } => {
                    self.pending_gone.remove(&id);
                    if self.selected_torrent_id == Some(id) {
                        self.detail = None;
                        self.files_torrent_id = None;
                    }
                    // Row already gone optimistically; re-SQL for catalog truth.
                    self.note_event(format!("#{id} deleted (soft)"));
                    self.request_catalog_list();
                }
                ControlEvent::SoftDeleteFailed { id, error } => {
                    self.pending_gone.remove(&id);
                    self.note_event(format!("#{id} delete failed: {error}"));
                    // Allow the row back via catalog list.
                    self.request_catalog_list();
                }
                ControlEvent::Removed { id } => {
                    self.pending_gone.remove(&id);
                    if self.selected_torrent_id == Some(id) {
                        self.detail = None;
                        self.files_torrent_id = None;
                    }
                    self.note_event(format!("#{id} removed"));
                    self.request_catalog_list();
                }
                ControlEvent::RemoveFailed { id, error } => {
                    self.pending_gone.remove(&id);
                    self.note_event(format!("#{id} remove failed: {error}"));
                    self.request_catalog_list();
                }
            }
        }
        (any, list_applied)
    }

    /// Refresh snapshot / optional SQL. Returns `true` if status or list changed.
    pub fn refresh(&mut self) -> Result<bool> {
        let (_any, mut list_changed) = self.poll_control_events();
        self.expire_row_ui();

        if let Some(c) = &self.control {
            if let Ok(snap) = c.snapshot() {
                // Lock miss: keep prior frame (real "stopped / no peers" is not lock_busy).
                if snap.lock_busy {
                    if !snap.status_line.is_empty() {
                        self.snap.status_line = snap.status_line;
                    }
                } else {
                    self.snap = snap;
                }
            }
        }

        // Full catalog list runs on catalog reader (ControlEvent::CatalogList).
        const SQL_INTERVAL: Duration = Duration::from_secs(5);
        if !self.db.exists() {
            self.rows.clear();
            self.status = format!("catalog missing: {}", self.db.display());
            self.last_refresh = Instant::now();
            return Ok(true);
        }
        if self.rows.is_empty() || self.last_sql.elapsed() >= SQL_INTERVAL {
            self.kick_catalog_list();
        }

        // Only patch rows that are hot — O(hot), not O(catalog).
        if !self.snap.torrents.is_empty() {
            let live: HashMap<i64, (u32, bool)> = self
                .snap
                .torrents
                .iter()
                .map(|t| (t.id, (t.have_count, t.complete)))
                .collect();
            for r in &mut self.rows {
                if matches!(self.row_ui.get(&r.id), Some(RowUiStatus::Rechecking { .. })) {
                    continue;
                }
                if let Some(&(have, complete)) = live.get(&r.id) {
                    r.have_count = have;
                    r.complete = complete;
                }
            }
        }

        // Keep detail tracker S/L / status fresh while the screen is open.
        if self.mode == Mode::Detail {
            if let Some(id) = self.detail.as_ref().map(|d| d.list.id) {
                if list_changed {
                    if let Ok(cat) = self.catalog() {
                        if let Ok(d) = cat.get_torrent_detail(id) {
                            self.detail = Some(d);
                        }
                    }
                }
            }
        }
        // Full 700-row rate re-sort is expensive; at most every 3s unless SQL just ran.
        if self.list_sort.needs_live_rates()
            && !self.snap.torrents.is_empty()
            && self.last_rate_sort.elapsed() >= Duration::from_secs(3)
        {
            self.apply_list_sort();
            self.restore_selection();
            self.last_rate_sort = Instant::now();
            list_changed = true;
        }
        // Critical sticky (disk dead) from engine snapshot — overrides everything.
        self.maybe_arm_sticky_from_engine();
        // Yellow status: events / exceptions only (healthy idle → blank).
        // Rates, peers, RUN live in the list/footer.
        self.status = self.compose_status_line();
        // Engine bind/listen failures land only on status_line — treat as sticky error.
        let eng = self.snap.status_line.trim();
        if status_msg_is_error(eng) && !is_disk_worker_dead_status(eng) && self.last_error != eng {
            self.last_error = eng.to_string();
            if self.status.is_empty() {
                self.status = self.last_error.clone();
            }
        }
        self.last_refresh = Instant::now();

        let sticky_key = self.sticky_status.as_deref().unwrap_or("");
        let status_key = format!("{}|{}|{}", self.status, self.last_error, sticky_key);
        // Progress + rates change every second while leeching; without this the
        // main loop keeps the prior frame and looks frozen between events.
        let snap_key = format!(
            "{}|{}|{}|{}|{}",
            self.snap.total_download_bps,
            self.snap.total_upload_bps,
            self.snap.total_session_down,
            self.snap.peers.len(),
            self.snap
                .torrents
                .iter()
                .map(|t| t.have_count as u64)
                .sum::<u64>()
        );
        let changed =
            list_changed || status_key != self.last_status_key || snap_key != self.last_snap_key;
        if changed {
            self.last_status_key = status_key;
            self.last_snap_key = snap_key;
        }
        Ok(changed)
    }

    /// Arm permanent sticky from engine snapshot (never cleared this process).
    pub(super) fn maybe_arm_sticky_from_engine(&mut self) {
        let eng = self.snap.status_line.trim();
        if !is_disk_worker_dead_status(eng) {
            return;
        }
        if self.sticky_status.as_deref() != Some(eng) {
            self.sticky_status = Some(eng.to_string());
        }
    }

    /// Event strip: critical sticky → recent event → selected exception → sticky error → empty.
    pub(super) fn compose_status_line(&self) -> String {
        // Permanent disk death: override all other status until process restart.
        if let Some(msg) = &self.sticky_status {
            return msg.clone();
        }

        // Quit path: always show engine status (stopped announce progress).
        if self.quitting {
            let eng = self.snap.status_line.trim();
            if !eng.is_empty() {
                return eng.to_string();
            }
            if !self.status.is_empty() {
                return self.status.clone();
            }
            return "quitting…".into();
        }

        const EVENT_HOLD: Duration = Duration::from_secs(6);

        if !self.last_event.is_empty() && self.last_event_at.elapsed() < EVENT_HOLD {
            return self.last_event.clone();
        }

        if let Some(id) = self.selected_id() {
            if let Some(ui) = self.row_ui.get(&id) {
                match ui {
                    RowUiStatus::Rechecking { good, piece_count } => {
                        return format!("#{id} rechecking {good}/{piece_count}");
                    }
                    RowUiStatus::RecheckFailed { at }
                        if at.elapsed() < Duration::from_secs(RowUiStatus::FLASH_SECS) =>
                    {
                        return format!("#{id} recheck failed");
                    }
                    RowUiStatus::RecheckDone { complete, at }
                        if at.elapsed() < Duration::from_secs(RowUiStatus::FLASH_SECS) =>
                    {
                        return if *complete {
                            format!("#{id} recheck ok (complete)")
                        } else {
                            format!("#{id} recheck ok (incomplete)")
                        };
                    }
                    _ => {}
                }
            }
            if let Some(r) = self.rows.iter().find(|r| r.id == id) {
                if r.state == "error" || r.state.starts_with("fail") {
                    return format!("#{id} state={}", r.state);
                }
            }
        }

        if !self.last_error.is_empty() {
            return self.last_error.clone();
        }

        // Idle healthy: blank (no seeding/announce chatter).
        String::new()
    }
}
