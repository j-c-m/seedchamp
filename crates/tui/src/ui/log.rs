//! Activity log screen.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::Frame;

use crate::app::App;

use super::panel_block;

pub(super) fn draw_log(f: &mut Frame, area: Rect, app: &mut App) {
    // borders (2) + panel horizontal padding (2)
    let inner_h = area.height.saturating_sub(2) as usize;
    let inner_w = area.width.saturating_sub(4) as usize;
    app.set_log_view_size(inner_w, inner_h);

    let filtered = app.filtered_log_lines();
    let n = filtered.len();
    let total = app.log_lines.len();
    let filt = app.log_filter.trim();
    let cap = app.activity.capture_filter();
    let filter_bit = if filt.is_empty() {
        String::new()
    } else {
        format!(" view={filt:?} {n}/{total}")
    };
    let follow = if app.log_from_end == 0 {
        "live"
    } else {
        "pinned"
    };
    let title = format!(" Activity log — {n} lines · {follow} · capture={cap}{filter_bit} ");

    let (start, end) = app.log_window_range(&filtered);
    let slice: &[&seedchamp_engine::LogLine] = if start < end && end <= filtered.len() {
        &filtered[start..end]
    } else {
        &[]
    };

    let th = &app.theme;
    let lines: Vec<Line> = if slice.is_empty() {
        vec![Line::from(Span::styled("—", th.muted_style()))]
    } else {
        slice
            .iter()
            .map(|e| {
                let (fg, label) = th.log_level(e.level);
                Line::from(vec![
                    Span::styled(format!("{} ", e.time), Style::default().fg(th.log_time)),
                    Span::styled(
                        format!("{label} "),
                        Style::default().fg(fg).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("{:<10} ", e.target),
                        Style::default().fg(th.log_target),
                    ),
                    Span::raw(e.message.clone()),
                ])
            })
            .collect()
    };

    f.render_widget(
        Paragraph::new(lines)
            .block(panel_block(title, &app.theme))
            .wrap(Wrap { trim: false }),
        area,
    );
}
