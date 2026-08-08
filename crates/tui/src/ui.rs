//! ratatui drawing.

mod detail;
mod files;
mod footer;
mod help;
mod input_popup;
mod list;
mod log;
mod peers;
mod status;

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Padding, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::{App, Mode};
use crate::theme::Theme;

use detail::draw_detail;
use files::draw_files;
use footer::{draw_footer, draw_status};
use help::{draw_help, draw_log_help};
use input_popup::draw_input_popup;
use list::draw_list;
use log::draw_log;
use peers::draw_peers;
use status::draw_status_screen;

pub(super) fn panel_block(
    title: impl Into<ratatui::text::Line<'static>>,
    theme: &Theme,
) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(theme.panel_border())
        .padding(Padding::horizontal(1))
        .title(title)
}

/// Inner content rows for a bordered panel (minus top/bottom border).
pub(super) fn panel_inner_h(area: Rect) -> u16 {
    area.height.saturating_sub(2).max(1)
}

/// Clamp + render a vertically scrollable text panel (Status / Help / Log help).
pub(super) fn render_scrollable_panel(
    f: &mut Frame,
    area: Rect,
    app: &mut App,
    title: impl Into<ratatui::text::Line<'static>>,
    lines: Vec<Line<'static>>,
) {
    let view_h = panel_inner_h(area);
    let content = lines.len() as u16;
    app.pane_view_h = view_h;
    app.pane_content_lines = content;
    let max = content.saturating_sub(view_h);
    if app.pane_scroll > max {
        app.pane_scroll = max;
    }
    f.render_widget(
        Paragraph::new(lines)
            .block(panel_block(title, &app.theme))
            .wrap(Wrap { trim: false })
            .scroll((app.pane_scroll, 0)),
        area,
    );
}

pub fn draw(f: &mut Frame, app: &mut App) {
    if let Some(bg) = app.theme.canvas_bg {
        f.render_widget(Block::default().style(Style::default().bg(bg)), f.area());
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(f.area());

    app.body_area = chunks[1];
    draw_header(f, chunks[0], app);
    match app.mode {
        Mode::List | Mode::Filter | Mode::Palette | Mode::Relocate => draw_list(f, chunks[1], app),
        Mode::Detail => draw_detail(f, chunks[1], app),
        Mode::Peers => draw_peers(f, chunks[1], app),
        Mode::Files => draw_files(f, chunks[1], app),
        Mode::Log | Mode::LogFilter => draw_log(f, chunks[1], app),
        Mode::LogHelp => draw_log_help(f, chunks[1], app),
        Mode::Help => draw_help(f, chunks[1], app),
        Mode::Status => draw_status_screen(f, chunks[1], app),
    }
    draw_status(f, chunks[2], app);
    draw_footer(f, chunks[3], app);

    if matches!(
        app.mode,
        Mode::Filter | Mode::LogFilter | Mode::Palette | Mode::Relocate
    ) {
        draw_input_popup(f, app);
    }
}

fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    let title = format!(
        " seedchamp  engine={}  db={} ",
        app.engine_version,
        app.db.display()
    );
    let p = Paragraph::new(title).style(app.theme.header_bar());
    f.render_widget(p, area);
}

pub fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if s.chars().count() <= max {
        return s.to_string();
    }
    if max == 1 {
        return "…".into();
    }
    let t: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{t}…")
}

pub(super) fn pct_or_done(have: u32, piece_count: u32) -> String {
    if piece_count == 0 {
        return "—".into();
    }
    if have >= piece_count {
        return "done".into();
    }
    let pct = (100.0 * have as f64 / piece_count as f64).floor() as u32;
    format!("{pct}%")
}

/// Right-align `s` into exactly `width` display columns (truncate if longer).
pub(super) fn pad_right(s: &str, width: usize) -> String {
    let n = s.chars().count();
    if n == width {
        return s.to_string();
    }
    if n > width {
        return s.chars().take(width).collect();
    }
    format!("{s:>width$}")
}
pub fn human_bytes(n: u64) -> String {
    const U: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n}{}", U[0])
    } else {
        format!("{v:.1}{}", U[i])
    }
}

pub(super) fn peer_is_active(p: &seedchamp_engine::PeerInfo) -> bool {
    p.download_bps > 0 || p.upload_bps > 0
}

/// `(active, total)` for one torrent (bound peers only).
pub(super) fn peer_counts_for_torrent(app: &App, torrent_id: i64) -> (usize, usize) {
    let mut total = 0usize;
    let mut active = 0usize;
    for p in &app.snap.peers {
        if p.torrent_id != torrent_id {
            continue;
        }
        total += 1;
        if peer_is_active(p) {
            active += 1;
        }
    }
    (active, total)
}

/// `(active, total)` across the session snapshot.
pub(super) fn peer_counts_global(app: &App) -> (usize, usize) {
    let total = app.snap.peers.len();
    let active = app.snap.peers.iter().filter(|p| peer_is_active(p)).count();
    (active, total)
}

/// `active/total` peer counts (e.g. `3/57`).
pub(super) fn format_peer_counts(active: usize, total: usize) -> String {
    format!("{active}/{total}")
}

/// Bytes/sec → short string e.g. `1.2MiB/s` or `0B/s` when idle.
pub fn rate_str(bps: u64) -> String {
    // Quantize so 43.7↔44.1 MiB/s doesn't thrash the label every frame.
    // bps==0 (and tiny rates that quantize to 0) → human_bytes(0) = "0B" → "0B/s".
    let q = quantize_bps(bps);
    format!("{}/s", human_bytes(q))
}

/// Filesystem I/O operations/sec (FreeBSD rusage inblock/oublock rates).
pub(super) fn ops_rate_str(ops_per_sec: u64) -> String {
    if ops_per_sec == 0 {
        "0/s".into()
    } else {
        format!("{ops_per_sec}/s")
    }
}

pub(super) fn quantize_bps(bps: u64) -> u64 {
    if bps < 10 * 1024 {
        // < 10 KiB/s: nearest 256 B
        (bps / 256) * 256
    } else if bps < 1024 * 1024 {
        // < 1 MiB/s: nearest 4 KiB
        (bps / 4096) * 4096
    } else if bps < 100 * 1024 * 1024 {
        // < 100 MiB/s: nearest 64 KiB (~0.06 MiB)
        (bps / (64 * 1024)) * (64 * 1024)
    } else {
        // very fast: nearest 256 KiB
        (bps / (256 * 1024)) * (256 * 1024)
    }
}

/// Upload/download ratio for display (`1.23`, `∞`, or `—`).
pub fn ratio_str(uploaded: u64, downloaded: u64) -> String {
    if downloaded == 0 {
        if uploaded == 0 {
            "—".into()
        } else {
            "∞".into()
        }
    } else {
        format!("{:.2}", uploaded as f64 / downloaded as f64)
    }
}
