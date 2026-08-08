//! Palette/colon commands and list mutations (start, delete, relocate, add).

use std::path::PathBuf;
use std::thread;

use seedchamp_engine::{add_torrent, AddOptions, Catalog, Result};

use crate::helpers::{default_data_root, parse_bps};
use crate::path_complete::{
    display_prefix_for_complete, list_path_completions, longest_common_prefix,
};

use super::{parse_id_or_selected, Mode, PaletteAction};

impl super::App {
    pub fn apply_filter(&mut self) -> Result<()> {
        self.filter = self.input.trim().to_string();
        self.clear_selection();
        self.request_catalog_list();
        self.refresh()?;
        Ok(())
    }

    /// Soft-delete selected torrent via control plane. Payload stays on disk.
    ///
    /// Requires stopped (`want_start` false). Engine rejects started torrents.
    pub fn delete_selected(&mut self) -> Result<()> {
        let Some(id) = self.selected_id() else {
            self.note_event("no torrent selected".into());
            return Ok(());
        };
        if self
            .rows
            .iter()
            .find(|r| r.id == id)
            .is_some_and(|r| r.want_start)
        {
            self.note_event(format!("#{id} delete failed: started"));
            return Ok(());
        }
        let Some(c) = self.require_control() else {
            self.note_event("control plane unavailable".into());
            return Ok(());
        };
        c.request_soft_delete(id)?;
        self.optimistic_remove_row(id);
        self.note_event(format!("#{id} delete → control"));
        Ok(())
    }

    pub fn toggle_start_selected(&mut self) -> Result<()> {
        let Some(id) = self.selected_id() else {
            return Ok(());
        };
        let want = self
            .rows
            .iter()
            .find(|r| r.id == id)
            .map(|r| !r.want_start)
            .unwrap_or(true);

        let Some(c) = self.require_control() else {
            self.note_event("control plane unavailable".into());
            return Ok(());
        };
        // Send only — reply arrives as ControlEvent on a later frame.
        if want {
            c.request_start(id)?;
            self.note_event(format!("#{id} start → control"));
        } else {
            c.request_stop(id)?;
            self.note_event(format!("#{id} stop → control"));
        }
        // Optimistic RUN flag until Started/Stopped event.
        if let Some(r) = self.rows.iter_mut().find(|r| r.id == id) {
            r.want_start = want;
        }
        Ok(())
    }

    pub fn recheck_selected(&mut self) -> Result<()> {
        let Some(id) = self.selected_id() else {
            return Ok(());
        };
        self.queue_recheck(id)
    }

    /// Open path prompt for Ctrl-O relocate (prefill current data_root or home when staged).
    pub fn begin_relocate(&mut self) -> Result<()> {
        let Some(id) = self.selected_id() else {
            self.note_event("no torrent selected".into());
            return Ok(());
        };
        let cat = Catalog::open(&self.db)?;
        let data_root = cat
            .get_data_root(id)
            .unwrap_or_else(|_| self.data_root.clone());
        let home = cat.get_home_root(id).ok().flatten();
        let staged = home.as_ref().map(|h| *h != data_root).unwrap_or(false);
        let prefill = if staged {
            home.unwrap().display().to_string()
        } else {
            data_root.display().to_string()
        };
        self.relocate_torrent_id = Some(id);
        self.input = prefill;
        self.clear_path_completion();
        self.mode = Mode::Relocate;
        self.status = format!("#{id}");
        Ok(())
    }

    pub fn clear_path_completion(&mut self) {
        self.path_completions.clear();
        self.path_completion_idx = 0;
        self.path_completion_base.clear();
    }

    /// Tab-complete filesystem path in `input` (directories preferred, cycle matches).
    pub fn tab_complete_path(&mut self) {
        let input = self.input.clone();
        // Rebuild candidate list when the typed prefix changed.
        if self.path_completions.is_empty() || self.path_completion_base != input {
            self.path_completions = list_path_completions(&input);
            self.path_completion_idx = 0;
            self.path_completion_base = input;
            if self.path_completions.is_empty() {
                self.status = "no path matches".into();
                return;
            }
            // First Tab: extend to longest common prefix of all matches when
            // that is strictly longer than the typed prefix; else take first.
            if self.path_completions.len() > 1 {
                let common = longest_common_prefix(&self.path_completions);
                let typed = display_prefix_for_complete(&self.input);
                if common.chars().count() > typed.chars().count() {
                    self.input = common;
                    self.path_completion_base = self.input.clone();
                    // Keep list so next Tab cycles full matches.
                    self.status = format!("{} matches — Tab to cycle", self.path_completions.len());
                    return;
                }
            }
        } else {
            // Subsequent Tab: cycle.
            self.path_completion_idx =
                (self.path_completion_idx + 1) % self.path_completions.len().max(1);
        }

        if let Some(choice) = self.path_completions.get(self.path_completion_idx).cloned() {
            self.input = choice;
            // After applying a full match, base becomes the choice so next Tab
            // continues cycling rather than rebuilding.
            self.path_completion_base = self.input.clone();
            if self.path_completions.len() == 1 {
                self.status = "path complete".into();
            } else {
                self.status = format!(
                    "match {}/{} — Tab next",
                    self.path_completion_idx + 1,
                    self.path_completions.len()
                );
            }
        }
    }

    /// Apply relocate from `input` (live via control plane; no stop/start).
    pub fn confirm_relocate(&mut self) -> Result<()> {
        let Some(id) = self.relocate_torrent_id else {
            self.mode = Mode::List;
            return Ok(());
        };
        let raw = self.input.trim().to_string();
        if raw.is_empty() {
            self.note_event("relocate: empty path".into());
            return Ok(());
        }
        let new_root = seedchamp_engine::expand_user_path(&raw);

        self.mode = Mode::List;
        self.relocate_torrent_id = None;
        self.input.clear();

        let Some(c) = self.require_control() else {
            self.note_event("control plane unavailable".into());
            return Ok(());
        };
        c.request_relocate(id, new_root.clone())?;
        self.note_event(format!("#{id} relocating → {}…", new_root.display()));
        Ok(())
    }

    pub(super) fn queue_recheck(&mut self, id: i64) -> Result<()> {
        self.mark_rechecking(id);
        let Some(c) = self.require_control() else {
            self.note_event("control plane unavailable".into());
            return Ok(());
        };
        c.request_recheck(id)?;
        self.note_event(format!("#{id} recheck…"));
        Ok(())
    }

    pub fn run_palette(&mut self) -> Result<PaletteAction> {
        let line = self.input.trim().to_string();
        if line.is_empty() {
            return Ok(PaletteAction::None);
        }
        let mut parts = line.split_whitespace();
        let cmd = parts.next().unwrap_or("").to_ascii_lowercase();
        match cmd.as_str() {
            "q" | "quit" | "exit" => return Ok(PaletteAction::Quit),
            "refresh" | "reload" => {
                self.request_catalog_list();
                self.refresh()?;
            }
            "filter" => {
                self.filter = parts.collect::<Vec<_>>().join(" ");
                self.clear_selection();
                self.request_catalog_list();
                self.refresh()?;
            }
            "clear" | "filterclear" => {
                self.filter.clear();
                self.request_catalog_list();
                self.refresh()?;
            }
            "start" => {
                if let Some(id) = parse_id_or_selected(parts.next(), self)? {
                    let Some(c) = self.require_control() else {
                        self.note_event("control plane unavailable".into());
                        return Ok(PaletteAction::None);
                    };
                    c.request_start(id)?;
                    self.note_event(format!("#{id} start → control"));
                }
            }
            "stop" => {
                if let Some(id) = parse_id_or_selected(parts.next(), self)? {
                    let Some(c) = self.require_control() else {
                        self.note_event("control plane unavailable".into());
                        return Ok(PaletteAction::None);
                    };
                    c.request_stop(id)?;
                    self.note_event(format!("#{id} stop → control"));
                }
            }
            "recheck" => {
                if let Some(id) = parse_id_or_selected(parts.next(), self)? {
                    self.queue_recheck(id)?;
                    // Async: RUN column shows chk until Rechecked event.
                }
            }
            "remove" | "rm" | "delete" => {
                if let Some(id) = parse_id_or_selected(parts.next(), self)? {
                    if self
                        .rows
                        .iter()
                        .find(|r| r.id == id)
                        .is_some_and(|r| r.want_start)
                    {
                        self.note_event(format!("#{id} remove failed: started"));
                        return Ok(PaletteAction::None);
                    }
                    let Some(c) = self.require_control() else {
                        self.note_event("control plane unavailable".into());
                        return Ok(PaletteAction::None);
                    };
                    c.request_remove(id)?;
                    self.optimistic_remove_row(id);
                    self.note_event(format!("#{id} remove → control"));
                }
            }
            "add" | "load" => {
                // :add <path|url> [start] [data=/path]
                let tokens: Vec<&str> = parts.collect();
                if tokens.is_empty() {
                    self.note_event("usage: :add <path|url> [start] [data=DIR]".into());
                } else {
                    self.cmd_add(&tokens)?;
                }
            }
            "peers" => self.mode = Mode::Peers,
            "files" | "file" => self.open_files()?,
            "detail" | "open" => self.open_detail()?,
            "log" | "logs" => {
                // :log                 → open log screen
                // :log debug           → set capture level (and open log)
                // :log seedchamp_engine=trace
                let rest: Vec<&str> = parts.collect();
                if rest.is_empty() {
                    self.open_log();
                } else {
                    self.set_log_capture(&rest.join(" "));
                    if self.mode != Mode::Log && self.mode != Mode::LogFilter {
                        self.open_log();
                    } else {
                        self.status = self.log_status_line();
                    }
                }
            }
            "loglevel" | "capture" => {
                let rest: Vec<&str> = parts.collect();
                if rest.is_empty() {
                    self.status = format!(
                        "capture={}  (usage: :loglevel debug|info|warn|error|trace)",
                        self.activity.capture_filter()
                    );
                } else {
                    self.set_log_capture(&rest.join(" "));
                }
            }
            "sort" | "view" | "screen" => {
                let arg = parts.next().unwrap_or("");
                if arg.is_empty() {
                    self.cycle_list_sort();
                } else {
                    self.set_list_sort(arg);
                }
            }
            "help" | "?" => self.mode = Mode::Help,
            "limits" | "limit" => {
                let mut lim = self.limits.clone();
                for tok in parts {
                    if let Some((k, v)) = tok.split_once('=') {
                        match k {
                            "up" | "upload" => lim.max_upload_bps = parse_bps(v),
                            "down" | "download" => lim.max_download_bps = parse_bps(v),
                            "peers" => lim.max_peers = v.parse().unwrap_or(lim.max_peers),
                            _ => {}
                        }
                    }
                }
                let Some(c) = self.require_control() else {
                    self.note_event("control plane unavailable".into());
                    return Ok(PaletteAction::None);
                };
                if let Err(e) = c.request_set_session_limits(lim) {
                    self.note_event(format!("limits failed: {e}"));
                }
            }
            other => self.status = format!("unknown: {other}"),
        }
        Ok(PaletteAction::None)
    }

    /// Load a .torrent from a local path or HTTP(S) URL (background thread).
    pub(super) fn cmd_add(&mut self, tokens: &[&str]) -> Result<()> {
        let mut source: Option<String> = None;
        let mut start = false;
        let mut data_root: Option<PathBuf> = None;
        for tok in tokens {
            if *tok == "start" {
                start = true;
            } else if let Some(d) = tok.strip_prefix("data=") {
                data_root = Some(PathBuf::from(d));
            } else if source.is_none() {
                source = Some((*tok).to_string());
            } else {
                // Allow paths/URLs with no spaces already split; join leftover as source error
                self.note_event(format!("add: unexpected token {tok:?}"));
                return Ok(());
            }
        }
        let Some(source) = source else {
            self.note_event("usage: :add <path|url> [start] [data=DIR]".into());
            return Ok(());
        };

        let data_root = data_root.unwrap_or_else(|| default_data_root(&self.db));
        let save_torrent_dir = Some(data_root.join(".torrents"));
        let opts = AddOptions {
            data_root,
            leech_cache: self.leech_cache.clone(),
            leech_cache_size: self.leech_cache_size,
            start,
            save_torrent_dir,
        };
        let db = self.db.clone();
        let control = self.control.clone();
        let bg = self.bg_tx.clone();
        self.note_event(format!("add… {source}"));

        thread::Builder::new()
            .name("seedchamp-tui-add".into())
            .spawn(move || {
                let msg = match Catalog::open(&db)
                    .and_then(|mut cat| add_torrent(&mut cat, &source, &opts))
                {
                    Ok(report) => {
                        let tag = if report.already_existed {
                            "exists"
                        } else {
                            "added"
                        };
                        if start {
                            if let Some(c) = &control {
                                let _ = c.request_start(report.id);
                            }
                        }
                        format!("add {tag} #{id} {name}", id = report.id, name = report.name)
                    }
                    Err(e) => format!("add failed: {e}"),
                };
                let _ = bg.send(msg);
            })
            .map_err(|e| seedchamp_engine::Error::Msg(format!("spawn add: {e}")))?;
        Ok(())
    }
}
