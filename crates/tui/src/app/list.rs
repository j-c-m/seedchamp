//! List selection, sort, detail/pane scroll, status/help open.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use seedchamp_engine::{collect_filesystem_usage, FilesystemUsage, Result};

use super::{scroll_apply, Mode, SortCriterion};

impl super::App {
    /// Open Status screen and take an immediate process sample (first paint).
    pub fn open_status(&mut self) {
        self.mode = Mode::Status;
        self.pane_scroll = 0;
        let _ = self.maybe_sample_process();
        self.status = "status".into();
    }

    pub fn open_help(&mut self) {
        self.mode = Mode::Help;
        self.pane_scroll = 0;
        self.status.clear();
    }

    pub fn open_log_help(&mut self) {
        self.mode = Mode::LogHelp;
        self.pane_scroll = 0;
        self.status.clear();
    }

    /// Sample process + filesystem metrics at most once per second.
    /// Never holds engine locks; uses already-loaded list rows for torrent roots.
    ///
    /// Returns `true` when a new sample was taken (caller should redraw).
    pub(super) fn maybe_sample_process(&mut self) -> bool {
        const INTERVAL: Duration = Duration::from_secs(1);
        if self.last_process_sample.elapsed() < INTERVAL {
            return false;
        }
        // Collect before updating the timestamp so a slow sample still throttles.
        self.process_sample = self.process_sample_state.collect();
        self.filesystems = self.collect_status_filesystems();
        self.last_process_sample = Instant::now();
        true
    }

    /// Default download FS always; plus FS for each open (`want_start`) torrent.
    pub(super) fn collect_status_filesystems(&self) -> Vec<FilesystemUsage> {
        let open_roots: Vec<PathBuf> = self
            .rows
            .iter()
            .filter(|r| r.want_start)
            .filter_map(|r| r.data_root.as_ref())
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .collect();
        collect_filesystem_usage(&self.data_root, open_roots)
    }
    pub fn selected_id(&self) -> Option<i64> {
        if let Some(id) = self.selected_torrent_id {
            return Some(id);
        }
        self.selected.and_then(|i| self.rows.get(i).map(|r| r.id))
    }

    /// Drop torrent from the visible list immediately (Ctrl-D / :remove).
    ///
    /// Call only after a successful control `request_*`. Selection stays on the
    /// same line via [`Self::restore_selection`]. Id stays in `pending_gone`
    /// until SoftDeleted/Removed or *Failed so CatalogList cannot reinsert it.
    pub(super) fn optimistic_remove_row(&mut self, id: i64) {
        self.pending_gone.insert(id);
        self.rows.retain(|r| r.id != id);
        self.row_ui.remove(&id);
        if self.selected_torrent_id == Some(id) {
            self.detail = None;
            self.files_torrent_id = None;
            // Keep selected_torrent_id so restore_selection pins the next row.
        }
        self.restore_selection();
    }

    pub fn live_for(&self, id: i64) -> Option<&seedchamp_engine::TorrentLive> {
        self.snap.torrents.iter().find(|t| t.id == id)
    }

    /// Pin highlight to `selected_torrent_id` after reloads/re-sorts.
    /// Leaves selection empty when the user has not chosen a row yet.
    pub(super) fn restore_selection(&mut self) {
        if self.rows.is_empty() {
            self.selected = None;
            self.selected_torrent_id = None;
            self.list_table_state.select(None);
            *self.list_table_state.offset_mut() = 0;
            return;
        }
        if let Some(id) = self.selected_torrent_id {
            if let Some(i) = self.rows.iter().position(|r| r.id == id) {
                self.selected = Some(i);
                self.list_table_state.select(Some(i));
                return;
            }
            // Selected torrent removed (e.g. Ctrl-D) — stay on the same line so
            // rows shift up under the cursor; do not jump scroll to top.
            let idx = self.selected.unwrap_or(0).min(self.rows.len() - 1);
            self.selected = Some(idx);
            self.selected_torrent_id = Some(self.rows[idx].id);
            self.list_table_state.select(Some(idx));
            return;
        }
        // Explicitly nothing selected (startup / user cleared).
        self.selected = None;
        self.list_table_state.select(None);
        // Keep offset as-is if user scrolled; only reset when truly unset at start.
    }

    pub(super) fn remember_selection(&mut self) {
        match self.selected {
            Some(i) => {
                self.selected_torrent_id = self.rows.get(i).map(|r| r.id);
                self.list_table_state.select(Some(i));
            }
            None => {
                self.selected_torrent_id = None;
                self.list_table_state.select(None);
            }
        }
    }

    /// Clear list selection (no highlight; view can sit at top).
    pub fn clear_selection(&mut self) {
        self.selected = None;
        self.selected_torrent_id = None;
        self.list_table_state.select(None);
        *self.list_table_state.offset_mut() = 0;
    }

    pub fn apply_list_sort(&mut self) {
        // Keep sticky id when we have one; do not invent a selection from index 0.
        if self.selected_torrent_id.is_none() {
            if let Some(i) = self.selected {
                self.selected_torrent_id = self.rows.get(i).map(|r| r.id);
            }
        }
        // O(1) rate lookups — avoid O(hot) find() on every comparison for 700 rows.
        let down: HashMap<i64, u64> = self
            .snap
            .torrents
            .iter()
            .map(|t| (t.id, t.download_bps))
            .collect();
        let up: HashMap<i64, u64> = self
            .snap
            .torrents
            .iter()
            .map(|t| (t.id, t.upload_bps))
            .collect();
        let order = self.list_sort.current().order.clone();
        self.rows.sort_by(|a, b| {
            use std::cmp::Ordering;
            for c in &order {
                let o = match c {
                    SortCriterion::OffFirst => {
                        // false (off) before true (on)
                        a.want_start.cmp(&b.want_start)
                    }
                    SortCriterion::DownRateDesc => {
                        let ad = down.get(&a.id).copied().unwrap_or(0);
                        let bd = down.get(&b.id).copied().unwrap_or(0);
                        bd.cmp(&ad)
                    }
                    SortCriterion::UpRateDesc => {
                        let au = up.get(&a.id).copied().unwrap_or(0);
                        let bu = up.get(&b.id).copied().unwrap_or(0);
                        bu.cmp(&au)
                    }
                    SortCriterion::AddedDesc => b.created_at.cmp(&a.created_at),
                    SortCriterion::NameAsc => a
                        .name
                        .to_ascii_lowercase()
                        .cmp(&b.name.to_ascii_lowercase()),
                    SortCriterion::IdAsc => a.id.cmp(&b.id),
                };
                if o != Ordering::Equal {
                    return o;
                }
            }
            Ordering::Equal
        });
        self.restore_selection();
    }

    pub fn cycle_list_sort(&mut self) {
        self.list_sort.cycle();
        self.apply_list_sort();
        self.note_event(format!("sort {}", self.list_sort.label()));
    }

    pub fn set_list_sort(&mut self, s: &str) {
        if !self.list_sort.set_by_key(s) {
            // Unknown key — ignore quietly (or keep current).
            self.note_event(format!("unknown sort {s:?}"));
            return;
        }
        self.apply_list_sort();
        self.note_event(format!("sort {}", self.list_sort.label()));
    }

    pub fn select_next(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        let next = match self.selected {
            None => 0, // first j/↓ from empty selection
            Some(i) => (i + 1).min(self.rows.len() - 1),
        };
        self.selected = Some(next);
        self.remember_selection();
    }

    /// Select absolute list row by index (e.g. mouse click). No-op if out of range.
    pub fn select_index(&mut self, idx: usize) {
        if idx >= self.rows.len() {
            return;
        }
        self.selected = Some(idx);
        self.remember_selection();
    }

    /// Clear highlight without jumping list scroll (header / empty area click).
    pub fn deselect_keep_scroll(&mut self) {
        self.selected = None;
        self.selected_torrent_id = None;
        self.list_table_state.select(None);
    }
    pub fn select_prev(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        match self.selected {
            None => {}                         // already on header
            Some(0) => self.clear_selection(), // first row → header
            Some(i) => {
                self.selected = Some(i - 1);
                self.remember_selection();
            }
        }
    }
    pub fn select_page(&mut self, dir: i32) {
        if self.rows.is_empty() {
            return;
        }
        let n = self.rows.len() as isize;
        let cur = self
            .selected
            .map(|i| i as isize)
            .unwrap_or(if dir > 0 { -1 } else { 0 });
        let mut s = cur + dir as isize * 10;
        s = s.clamp(0, n - 1);
        self.selected = Some(s as usize);
        self.remember_selection();
    }
    pub fn select_first(&mut self) {
        if self.rows.is_empty() {
            self.clear_selection();
            return;
        }
        self.selected = Some(0);
        self.remember_selection();
    }
    pub fn select_last(&mut self) {
        if self.rows.is_empty() {
            self.clear_selection();
            return;
        }
        self.selected = Some(self.rows.len() - 1);
        self.remember_selection();
    }

    pub fn open_detail(&mut self) -> Result<()> {
        let Some(id) = self.selected_id() else {
            self.status = "no torrent selected".into();
            return Ok(());
        };
        let d = {
            let cat = self.catalog()?;
            cat.get_torrent_detail(id)?
        };
        self.detail = Some(d);
        self.detail_scroll = 0;
        self.detail_content_lines = 0;
        self.mode = Mode::Detail;
        Ok(())
    }
    pub fn detail_scroll(&mut self, delta: i16) {
        self.detail_scroll = scroll_apply(self.detail_scroll, delta, self.detail_max_scroll());
    }

    pub fn detail_scroll_page(&mut self, dir: i16) {
        let step = self.detail_view_h.saturating_sub(1).max(1) as i16;
        self.detail_scroll(step.saturating_mul(dir.signum()));
    }

    pub fn detail_scroll_home(&mut self) {
        self.detail_scroll = 0;
    }

    pub fn detail_scroll_end(&mut self) {
        self.detail_scroll = self.detail_max_scroll();
    }

    pub(super) fn detail_max_scroll(&self) -> u16 {
        self.detail_content_lines
            .saturating_sub(self.detail_view_h.max(1))
    }

    /// Scroll Status / Help / Log help text panels.
    pub fn pane_scroll(&mut self, delta: i16) {
        self.pane_scroll = scroll_apply(self.pane_scroll, delta, self.pane_max_scroll());
    }

    pub fn pane_scroll_page(&mut self, dir: i16) {
        let step = self.pane_view_h.saturating_sub(1).max(1) as i16;
        self.pane_scroll(step.saturating_mul(dir.signum()));
    }

    pub fn pane_scroll_home(&mut self) {
        self.pane_scroll = 0;
    }

    pub fn pane_scroll_end(&mut self) {
        self.pane_scroll = self.pane_max_scroll();
    }

    pub(super) fn pane_max_scroll(&self) -> u16 {
        self.pane_content_lines
            .saturating_sub(self.pane_view_h.max(1))
    }
}
