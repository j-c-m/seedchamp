//! Per-torrent files tree screen.

use seedchamp_engine::Result;

use crate::file_tree::{self, FileTreeRow};

use super::Mode;

impl super::App {
    pub fn rebuild_file_tree(&mut self) {
        self.file_tree = file_tree::build_visible_rows(&self.files, &self.file_collapsed);
        if self.file_tree.is_empty() {
            self.file_selected = 0;
        } else if self.file_selected >= self.file_tree.len() {
            self.file_selected = self.file_tree.len() - 1;
        }
        self.sync_file_table_state();
    }

    /// Keep files-table scroll/selection in sync with `file_selected`.
    pub fn sync_file_table_state(&mut self) {
        if self.file_tree.is_empty() {
            self.file_table_state.select(None);
        } else {
            self.file_table_state
                .select(Some(self.file_selected.min(self.file_tree.len() - 1)));
        }
    }

    pub fn close_files(&mut self) {
        self.mode = Mode::List;
        self.files.clear();
        self.file_tree.clear();
        self.file_collapsed.clear();
        self.files_torrent_id = None;
        self.file_selected = 0;
        self.file_table_state.select(None);
    }

    pub fn open_files(&mut self) -> Result<()> {
        let Some(id) = self.selected_id() else {
            self.note_event("no torrent selected".into());
            return Ok(());
        };
        let files = match self.catalog() {
            Ok(cat) => cat.list_files_progress(id),
            Err(e) => {
                self.note_event(format!("catalog: {e}"));
                return Ok(());
            }
        };
        match files {
            Ok(f) => self.files = f,
            Err(e) => {
                self.note_event(format!("files: {e}"));
                return Ok(());
            }
        }
        self.files_torrent_id = Some(id);
        self.file_collapsed.clear();
        self.file_selected = 0;
        self.rebuild_file_tree();
        self.mode = Mode::Files;
        Ok(())
    }

    pub fn refresh_files(&mut self) -> Result<()> {
        let Some(id) = self.files_torrent_id else {
            return Ok(());
        };
        // Reuse short-timeout UI catalog; skip frame on SQLITE_BUSY rather than hang.
        let files = match self.catalog() {
            Ok(cat) => cat.list_files_progress(id),
            Err(e) => {
                tracing::debug!(error = %e, "files refresh: catalog open");
                return Ok(());
            }
        };
        match files {
            Ok(f) => {
                self.files = f;
                self.rebuild_file_tree();
            }
            Err(e) => {
                // Busy / transient — keep prior file % rather than freeze TUI.
                tracing::debug!(error = %e, "files refresh skipped");
            }
        }
        Ok(())
    }

    pub fn file_select_delta(&mut self, delta: i32) {
        if self.file_tree.is_empty() {
            return;
        }
        let n = self.file_tree.len() as i32;
        let cur = self.file_selected as i32;
        let next = (cur + delta).rem_euclid(n);
        self.file_selected = next as usize;
        self.sync_file_table_state();
    }

    pub fn file_select_first(&mut self) {
        self.file_selected = 0;
        self.sync_file_table_state();
    }

    pub fn file_select_last(&mut self) {
        if !self.file_tree.is_empty() {
            self.file_selected = self.file_tree.len() - 1;
            self.sync_file_table_state();
        }
    }

    /// Expand/collapse the selected directory row.
    pub fn toggle_file_dir_expand(&mut self) {
        let Some(FileTreeRow::Dir {
            prefix, expanded, ..
        }) = self.file_tree.get(self.file_selected).cloned()
        else {
            return;
        };
        if expanded {
            self.file_collapsed.insert(prefix);
        } else {
            self.file_collapsed.remove(&prefix);
        }
        self.rebuild_file_tree();
    }

    /// Toggle selected file or all files under a directory (`priority` 0 ↔ 1).
    pub fn toggle_file_selected(&mut self) -> Result<()> {
        let Some(tid) = self.files_torrent_id else {
            return Ok(());
        };
        let Some(row) = self.file_tree.get(self.file_selected).cloned() else {
            return Ok(());
        };
        let (indices, new_prio, label): (Vec<usize>, i32, String) = match row {
            FileTreeRow::File { file_index, .. } => {
                let Some(fp) = self.files.get(file_index) else {
                    return Ok(());
                };
                let new_prio = if fp.wanted() { 0 } else { 1 };
                (vec![file_index], new_prio, format!("file {}", fp.file.idx))
            }
            FileTreeRow::Dir {
                name,
                file_indices,
                wanted,
                ..
            } => {
                // Mixed or all-on → turn off; all-off → turn on.
                let new_prio = match wanted {
                    file_tree::DirWanted::None => 1,
                    file_tree::DirWanted::All | file_tree::DirWanted::Mixed => 0,
                };
                (file_indices, new_prio, format!("dir {name}"))
            }
        };
        if indices.is_empty() {
            return Ok(());
        }
        for &fi in &indices {
            let Some(fp) = self.files.get(fi) else {
                continue;
            };
            let fidx = fp.file.idx;
            let Some(c) = self.require_control() else {
                self.note_event("control plane unavailable".into());
                return Ok(());
            };
            c.request_set_file_priority(tid, fidx, new_prio)?;
            if let Some(row) = self.files.get_mut(fi) {
                row.file.priority = new_prio;
            }
        }
        let on = if new_prio > 0 { "on" } else { "off" };
        self.note_event(format!("#{tid} {label} {on} (×{})", indices.len()));
        self.rebuild_file_tree();
        Ok(())
    }
}
