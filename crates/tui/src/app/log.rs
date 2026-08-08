//! Activity log screen: filter, scroll, capture level.

use seedchamp_engine::LogLine;

use super::{level_label, level_rank, Mode};

impl super::App {
    /// Refresh cached log lines if the ring advanced. Returns true when UI should redraw.
    pub fn poll_activity_log(&mut self) -> bool {
        let seq = self.activity.seq();
        if seq == self.last_log_seq {
            return false;
        }
        self.last_log_seq = seq;
        self.log_lines = self.activity.snapshot();
        self.reanchor_log_after_snapshot();
        if self.mode == Mode::Log {
            self.status = self.log_status_line();
        }
        true
    }

    pub fn open_log(&mut self) {
        self.mode = Mode::Log;
        self.log_follow_tail();
        let _ = self.poll_activity_log();
        // Force snapshot even if seq unchanged (first open).
        self.log_lines = self.activity.snapshot();
        self.last_log_seq = self.activity.seq();
        self.status = self.log_status_line();
    }

    pub fn close_log(&mut self) {
        self.mode = Mode::List;
        self.status.clear();
    }

    pub fn begin_log_filter(&mut self) {
        self.mode = Mode::LogFilter;
        self.input = self.log_filter.clone();
        self.status = "log filter".into();
    }

    pub fn apply_log_filter(&mut self) {
        self.log_filter = self.input.trim().to_string();
        self.input.clear();
        self.mode = Mode::Log;
        self.log_follow_tail(); // jump to live tail of filtered view
        self.status = self.log_status_line();
    }

    pub fn cancel_log_filter_prompt(&mut self) {
        self.input.clear();
        self.mode = Mode::Log;
        self.status = self.log_status_line();
    }

    pub fn clear_log_filter(&mut self) {
        self.log_filter.clear();
        self.log_follow_tail();
        self.status = self.log_status_line();
    }

    pub fn cycle_log_capture(&mut self) {
        match self.activity.cycle_capture_level() {
            Ok(level) => {
                self.status =
                    format!("log capture → {level}  (v cycle · :log debug · / is display-only)");
            }
            Err(e) => {
                self.status = format!("log capture failed: {e}");
            }
        }
        let _ = self.poll_activity_log();
    }

    pub fn set_log_capture(&mut self, directive: &str) {
        match self.activity.set_capture_filter(directive) {
            Ok(()) => {
                self.status = format!("log capture → {}", self.activity.capture_filter());
            }
            Err(e) => {
                self.status = format!("log capture failed: {e}");
            }
        }
        let _ = self.poll_activity_log();
    }

    pub fn log_status_line(&self) -> String {
        let total = self.log_lines.len();
        let shown = self.filtered_log_count();
        let cap = self.activity.capture_filter();
        let follow = if self.log_from_end == 0 {
            "follow".to_string()
        } else {
            format!("↑{} pinned", self.log_from_end)
        };
        if self.log_filter.is_empty() {
            format!("log — {total} lines · {follow} · capture={cap}")
        } else {
            format!(
                "log — {shown}/{total} match {:?} · {follow} · capture={cap}",
                self.log_filter
            )
        }
    }

    /// Whether a ring line is visible under the current display filter.
    pub fn log_line_matches(&self, line: &LogLine) -> bool {
        let f = self.log_filter.trim();
        if f.is_empty() {
            return true;
        }
        let f = f.to_ascii_lowercase();
        // Single-letter level filter: e w i d t (and full names).
        if matches!(
            f.as_str(),
            "e" | "err"
                | "error"
                | "w"
                | "wrn"
                | "warn"
                | "warning"
                | "i"
                | "inf"
                | "info"
                | "d"
                | "dbg"
                | "debug"
                | "t"
                | "trc"
                | "trace"
        ) {
            let want = match f.chars().next().unwrap() {
                'e' => 'E',
                'w' => 'W',
                'i' => 'I',
                'd' => 'D',
                't' => 'T',
                _ => 'I',
            };
            return line.level == want;
        }
        // Minimum level: `>=w` / `>=warn` → warn + error.
        if let Some(rest) = f.strip_prefix(">=") {
            let min = match rest.trim() {
                "e" | "err" | "error" => 0u8,
                "w" | "wrn" | "warn" | "warning" => 1,
                "i" | "inf" | "info" => 2,
                "d" | "dbg" | "debug" => 3,
                "t" | "trc" | "trace" => 4,
                _ => return self.log_haystack(line).contains(&f),
            };
            return level_rank(line.level) <= min;
        }
        self.log_haystack(line).contains(&f)
    }

    pub(super) fn log_haystack(&self, line: &LogLine) -> String {
        format!(
            "{} {} {} {} {}",
            line.time,
            line.level,
            level_label(line.level),
            line.target,
            line.message
        )
        .to_ascii_lowercase()
    }

    pub fn filtered_log_count(&self) -> usize {
        if self.log_filter.trim().is_empty() {
            return self.log_lines.len();
        }
        self.log_lines
            .iter()
            .filter(|l| self.log_line_matches(l))
            .count()
    }

    /// Filtered lines oldest→newest for drawing / scroll.
    pub fn filtered_log_lines(&self) -> Vec<&LogLine> {
        if self.log_filter.trim().is_empty() {
            return self.log_lines.iter().collect();
        }
        self.log_lines
            .iter()
            .filter(|l| self.log_line_matches(l))
            .collect()
    }

    /// Scroll log by **entries**: positive = older (up), negative = newer (toward live tail).
    pub fn log_scroll(&mut self, delta: i32) {
        let max_up = {
            let filtered = self.filtered_log_lines();
            if filtered.is_empty() {
                self.log_follow_tail();
                self.status = self.log_status_line();
                return;
            }
            self.max_log_from_end(&filtered)
        };
        if delta > 0 {
            self.log_from_end = (self.log_from_end + delta as usize).min(max_up);
        } else {
            let d = (-delta) as usize;
            self.log_from_end = self.log_from_end.saturating_sub(d);
        }
        self.log_anchor_seq = if self.log_from_end == 0 {
            None
        } else {
            self.top_visible_log_seq()
        };
        self.status = self.log_status_line();
    }

    /// Page step: viewport height minus a couple rows of context.
    pub fn log_page_delta(&self) -> i32 {
        self.log_view_h.saturating_sub(2).max(1) as i32
    }

    pub fn log_scroll_home(&mut self) {
        let max_up = {
            let filtered = self.filtered_log_lines();
            self.max_log_from_end(&filtered)
        };
        self.log_from_end = max_up;
        self.log_anchor_seq = if self.log_from_end == 0 {
            None
        } else {
            self.top_visible_log_seq()
        };
        self.status = self.log_status_line();
    }

    pub fn log_scroll_end(&mut self) {
        self.log_follow_tail();
        self.status = self.log_status_line();
    }

    pub(super) fn log_follow_tail(&mut self) {
        self.log_from_end = 0;
        self.log_anchor_seq = None;
    }

    /// Visual rows for one log entry at the given content width (matches `draw_log` layout).
    pub fn log_entry_rows(line: &LogLine, width: usize) -> usize {
        // Prefix: "HH:MM:SS " (9) + "ERR " (4) + "{target:<10} " (11) ≈ 24.
        const PREFIX: usize = 24;
        let msg = line.message.chars().count();
        let total = PREFIX.saturating_add(msg).max(1);
        let w = width.max(1);
        total.div_ceil(w).max(1)
    }

    /// Record panel geometry from the last `draw_log` (used for page/Home/clamp).
    pub fn set_log_view_size(&mut self, width: usize, height: usize) {
        self.log_view_w = width.max(1);
        self.log_view_h = height.max(1);
    }

    /// `(start, end)` into `filtered` for the current scroll offset (wrap-aware).
    pub fn log_window_range(&self, filtered: &[&LogLine]) -> (usize, usize) {
        Self::compute_log_window(
            filtered,
            self.log_from_end,
            self.log_view_w,
            self.log_view_h,
        )
    }

    pub(super) fn compute_log_window(
        filtered: &[&LogLine],
        from_end: usize,
        width: usize,
        height: usize,
    ) -> (usize, usize) {
        let n = filtered.len();
        if n == 0 || height == 0 {
            return (0, 0);
        }
        let from_end = from_end.min(n);
        let end = n.saturating_sub(from_end);
        // Pack upward from `end` until visual height is filled.
        let mut start = end;
        let mut rows = 0usize;
        while start > 0 {
            let r = Self::log_entry_rows(filtered[start - 1], width);
            if rows > 0 && rows.saturating_add(r) > height {
                break;
            }
            start -= 1;
            rows = rows.saturating_add(r);
            if rows >= height {
                break;
            }
        }
        // If everything fits and from_end forced a hole, show from start of buffer.
        if from_end == 0 && end == n && start > 0 {
            // follow already packs from bottom — fine
        }
        let _ = end; // end may be 0 if from_end == n
        if start >= end && n > 0 {
            // Degenerate: show at least one line at the bottom of the requested region.
            let idx = end.saturating_sub(1).min(n - 1);
            return (idx, idx + 1);
        }
        (start, end)
    }

    /// Max `log_from_end` so Home shows a **full page of oldest** lines (not one line).
    pub(super) fn max_log_from_end(&self, filtered: &[&LogLine]) -> usize {
        let n = filtered.len();
        if n == 0 {
            return 0;
        }
        let height = self.log_view_h.max(1);
        let width = self.log_view_w.max(1);
        // Pack from the oldest end until the viewport is full.
        let mut end = 0usize;
        let mut rows = 0usize;
        while end < n {
            let r = Self::log_entry_rows(filtered[end], width);
            if rows > 0 && rows.saturating_add(r) > height {
                break;
            }
            rows = rows.saturating_add(r);
            end += 1;
            if rows >= height {
                break;
            }
        }
        // If the whole buffer fits, stay in follow (max offset 0).
        if end >= n {
            return 0;
        }
        n.saturating_sub(end)
    }

    /// Seq of the top visible line for the current `log_from_end` (pin target).
    pub(super) fn top_visible_log_seq(&self) -> Option<u64> {
        let filtered = self.filtered_log_lines();
        let (start, end) = Self::compute_log_window(
            &filtered,
            self.log_from_end,
            self.log_view_w,
            self.log_view_h,
        );
        if start < end {
            Some(filtered[start].seq)
        } else {
            None
        }
    }

    /// After a ring snapshot: keep pinned content stable, or clamp follow.
    pub(super) fn reanchor_log_after_snapshot(&mut self) {
        if self.log_from_end == 0 || self.log_anchor_seq.is_none() {
            self.log_from_end = 0;
            self.log_anchor_seq = None;
            return;
        }
        let seq = self.log_anchor_seq.unwrap();
        let height = self.log_view_h.max(1);
        let width = self.log_view_w.max(1);

        // Compute new offset without holding a borrow across assignment.
        let outcome = {
            let filtered = self.filtered_log_lines();
            let n = filtered.len();
            if n == 0 {
                None
            } else if let Some(idx) = filtered.iter().position(|l| l.seq == seq) {
                let mut end = idx;
                let mut rows = 0usize;
                while end < n {
                    let r = Self::log_entry_rows(filtered[end], width);
                    if rows > 0 && rows.saturating_add(r) > height {
                        break;
                    }
                    rows = rows.saturating_add(r);
                    end += 1;
                    if rows >= height {
                        break;
                    }
                }
                let mut start = idx;
                while start > 0 && rows < height {
                    let r = Self::log_entry_rows(filtered[start - 1], width);
                    if rows.saturating_add(r) > height {
                        break;
                    }
                    start -= 1;
                    rows = rows.saturating_add(r);
                }
                Some((n.saturating_sub(end), Some(filtered[start].seq)))
            } else {
                // Anchored line left the ring / filter — oldest full page.
                let max_up = self.max_log_from_end(&filtered);
                if max_up == 0 {
                    Some((0, None))
                } else {
                    let (start, _) = Self::compute_log_window(&filtered, max_up, width, height);
                    let anch = filtered.get(start).map(|l| l.seq);
                    Some((max_up, anch))
                }
            }
        };

        match outcome {
            None => self.log_follow_tail(),
            Some((from_end, anch)) => {
                self.log_from_end = from_end;
                self.log_anchor_seq = anch;
            }
        }
    }
}
