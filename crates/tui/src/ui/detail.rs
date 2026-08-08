//! Torrent detail panel.

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::Frame;

use crate::app::App;
use crate::theme::Theme;

use super::{
    format_peer_counts, human_bytes, panel_block, panel_inner_h, pct_or_done,
    peer_counts_for_torrent, rate_str, ratio_str,
};

pub(super) fn draw_detail(f: &mut Frame, area: Rect, app: &mut App) {
    let Some(d) = &app.detail else {
        f.render_widget(Paragraph::new("—").block(panel_block("", &app.theme)), area);
        return;
    };
    let th = &app.theme;
    let r = &d.list;
    let live = app.live_for(r.id);
    let mut lines: Vec<Line> = Vec::new();

    // ── Title ──────────────────────────────────────────────────────────
    lines.push(Line::from(Span::styled(r.name.clone(), th.accent_style())));
    lines.push(kv_line(
        "infohash",
        Span::styled(r.infohash_hex.clone(), th.muted_style()),
        th,
    ));
    lines.push(Line::from(""));

    // ── Status ─────────────────────────────────────────────────────────
    lines.push(section_header("Status", th));
    let (have, piece_count, complete) = if let Some(l) = live {
        (l.have_count, l.piece_count, l.complete)
    } else {
        (r.have_count, r.piece_count, r.complete)
    };
    let pct = if complete {
        "done".to_string()
    } else {
        pct_or_done(have, piece_count)
    };
    let run = if r.want_start { "on" } else { "off" };
    let private = if d.private { "yes" } else { "no" };
    lines.push(kv_line(
        "id / state",
        Span::raw(format!(
            "#{}  {}  want={}  private={}",
            r.id, r.state, run, private
        )),
        th,
    ));
    lines.push(kv_line(
        "pieces",
        Span::raw(format!(
            "{have}/{piece_count}  ({pct})  piece_len {}",
            human_bytes(d.piece_length as u64)
        )),
        th,
    ));
    if let Some(l) = live {
        lines.push(kv_line(
            "left",
            Span::raw(if l.left == 0 {
                "0".into()
            } else {
                human_bytes(l.left)
            }),
            th,
        ));
    }
    if d.corrupted > 0 {
        lines.push(kv_line(
            "corrupted",
            Span::styled(human_bytes(d.corrupted), Style::default().fg(th.warn)),
            th,
        ));
    }
    lines.push(Line::from(""));

    // ── Transfer ───────────────────────────────────────────────────────
    lines.push(section_header("Transfer", th));
    let (up, dn) = if let Some(l) = live {
        (
            r.uploaded.max(l.lifetime_uploaded),
            r.downloaded.max(l.completed_bytes),
        )
    } else {
        (r.uploaded, r.downloaded)
    };
    lines.push(kv_line(
        "size",
        Span::raw(format!(
            "{}  files={}  ratio {}",
            human_bytes(r.total_size),
            d.files.len(),
            ratio_str(up, dn)
        )),
        th,
    ));
    lines.push(kv_line(
        "lifetime",
        Span::raw(format!("DN {}  UP {}", human_bytes(dn), human_bytes(up))),
        th,
    ));
    if let Some(l) = live {
        let (pa, pt) = peer_counts_for_torrent(app, r.id);
        lines.push(kv_line(
            "session",
            Span::raw(format!(
                "DN {}  UP {}",
                human_bytes(l.session_downloaded),
                human_bytes(l.session_uploaded)
            )),
            th,
        ));
        lines.push(kv_line(
            "rates",
            Span::raw(format!(
                "↓ {}  ↑ {}",
                rate_str(l.download_bps),
                rate_str(l.upload_bps)
            )),
            th,
        ));
        lines.push(kv_line("peers", Span::raw(format_peer_counts(pa, pt)), th));
    } else {
        lines.push(kv_line("live", Span::styled("—", th.muted_style()), th));
    }
    lines.push(Line::from(""));

    lines.push(section_header("Swarm", th));
    let (swarm_s, swarm_l) = swarm_sl_for_detail(d, live);
    lines.push(kv_line(
        "S / L",
        Span::raw(format_swarm_sl(swarm_s, swarm_l)),
        th,
    ));
    if let Some(l) = live {
        lines.push(kv_line("announce", Span::raw(announce_timer_str(l)), th));
    } else {
        lines.push(kv_line("announce", Span::styled("—", th.muted_style()), th));
    }
    lines.push(Line::from(""));

    lines.push(section_header("Paths", th));
    if let Some(root) = &r.data_root {
        lines.push(kv_line("data_root", Span::raw(root.clone()), th));
    } else {
        lines.push(kv_line(
            "data_root",
            Span::styled("—", th.muted_style()),
            th,
        ));
    }
    if let Some(src) = &d.source_torrent {
        lines.push(kv_line("source", Span::raw(src.clone()), th));
    }
    lines.push(kv_line(
        "added",
        Span::raw(format_unix_time(r.created_at)),
        th,
    ));
    lines.push(kv_line(
        "finished",
        Span::raw(
            d.finished_at
                .map(format_unix_time)
                .unwrap_or_else(|| "—".into()),
        ),
        th,
    ));

    if let Some(err) = &d.error_msg {
        if !err.is_empty() {
            lines.push(Line::from(""));
            lines.push(section_header("Error", th));
            lines.push(Line::from(Span::styled(
                format!("  {err}"),
                Style::default().fg(th.error),
            )));
        }
    }

    // ── Trackers ───────────────────────────────────────────────────────
    lines.push(Line::from(""));
    lines.push(section_header("Trackers", th));
    if d.trackers.is_empty() {
        lines.push(Line::from(Span::styled("  (none)", th.muted_style())));
    } else {
        // Column header
        lines.push(Line::from(Span::styled(
            format!(
                "  {:<4} {:<5} {:>6} {:>6} {:>6}  {}",
                "tier", "en", "S", "L", "peers", "status"
            ),
            th.muted_style(),
        )));
        for t in &d.trackers {
            let en = if t.enabled { "on" } else { "off" };
            let s = opt_u32_cell(t.seeders);
            let lch = opt_u32_cell(t.leechers);
            let peers = opt_u32_cell(t.last_peers);
            let status = tracker_status_display(t);
            let status_style = tracker_status_style(t, th);
            lines.push(Line::from(vec![
                Span::raw(format!(
                    "  {:<4} {:<5} {:>6} {:>6} {:>6}  ",
                    t.tier, en, s, lch, peers
                )),
                Span::styled(status, status_style),
            ]));
            lines.push(Line::from(Span::styled(
                format!("         {}", t.url),
                th.muted_style(),
            )));
        }
    }

    let view_h = panel_inner_h(area);
    let content = lines.len() as u16;
    app.detail_view_h = view_h;
    app.detail_content_lines = content;
    let max = content.saturating_sub(view_h);
    if app.detail_scroll > max {
        app.detail_scroll = max;
    }
    let p = Paragraph::new(lines)
        .block(panel_block(" Detail ", &app.theme))
        .wrap(Wrap { trim: false })
        .scroll((app.detail_scroll, 0));
    f.render_widget(p, area);
}

pub(super) fn section_header(title: &str, theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(title.to_string(), theme.section_style()))
}

/// Label column width for detail key/value rows.
const DETAIL_LABEL_W: usize = 12;

pub(super) fn kv_line(label: &str, value: Span<'static>, theme: &Theme) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("  {label:<width$} ", width = DETAIL_LABEL_W),
            theme.label_style(),
        ),
        value,
    ])
}

pub(super) fn swarm_sl_for_detail(
    d: &seedchamp_engine::TorrentDetail,
    live: Option<&seedchamp_engine::TorrentLive>,
) -> (Option<u32>, Option<u32>) {
    if let Some(l) = live {
        if l.seeders.is_some() || l.leechers.is_some() {
            return (l.seeders, l.leechers);
        }
    }
    d.swarm_sl()
}

pub(super) fn format_swarm_sl(seeders: Option<u32>, leechers: Option<u32>) -> String {
    match (seeders, leechers) {
        (Some(s), Some(l)) => format!("{s} seeders  /  {l} leechers"),
        (Some(s), None) => format!("{s} seeders  /  — leechers"),
        (None, Some(l)) => format!("— seeders  /  {l} leechers"),
        (None, None) => "—".into(),
    }
}

pub(super) fn opt_u32_cell(v: Option<u32>) -> String {
    match v {
        Some(n) => n.to_string(),
        None => "—".into(),
    }
}

pub(super) fn tracker_status_display(t: &seedchamp_engine::TrackerRow) -> String {
    let age = t
        .last_announce_at
        .map(format_unix_age)
        .unwrap_or_else(|| "never".into());
    match t.last_status.as_deref() {
        None => format!("—  ({age})"),
        Some("ok") => {
            let iv = t
                .last_interval
                .map(|s| format!(" iv {}", format_duration_secs(s)))
                .unwrap_or_default();
            format!("ok{iv}  ({age})")
        }
        Some(s) => {
            let short = if s.chars().count() > 40 {
                format!("{}…", s.chars().take(39).collect::<String>())
            } else {
                s.to_string()
            };
            format!("{short}  ({age})")
        }
    }
}

pub(super) fn tracker_status_style(t: &seedchamp_engine::TrackerRow, theme: &Theme) -> Style {
    match t.last_status.as_deref() {
        Some("ok") => Style::default().fg(theme.ok),
        Some(_) => Style::default().fg(theme.error),
        None => theme.muted_style(),
    }
}

pub(super) fn format_unix_time(secs: i64) -> String {
    if secs <= 0 {
        return "—".into();
    }
    // UTC date-time without external deps (YYYY-MM-DD HH:MM:SS).
    let s = secs as u64;
    let days = s / 86400;
    let rem = s % 86400;
    let h = rem / 3600;
    let m = (rem % 3600) / 60;
    let sec = rem % 60;
    let (y, mo, d) = civil_from_days(days as i64);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02}:{sec:02} UTC")
}

pub(super) fn format_unix_age(secs: i64) -> String {
    if secs <= 0 {
        return "—".into();
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(secs);
    let age = (now - secs).max(0) as u32;
    if age < 5 {
        "just now".into()
    } else {
        format!("{} ago", format_duration_secs(age))
    }
}

/// Howard Hinnant civil_from_days (proleptic Gregorian).
pub(super) fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m as u32, d as u32)
}

/// Next announce countdown for a hot torrent.
pub(super) fn announce_timer_str(l: &seedchamp_engine::TorrentLive) -> String {
    if l.announce_in_flight {
        return "in flight…".into();
    }
    match (l.announce_in_secs, l.announce_interval_secs) {
        (Some(0), Some(iv)) => format!("due now  (interval {})", format_duration_secs(iv)),
        (Some(secs), Some(iv)) => format!(
            "in {}  (interval {})",
            format_duration_secs(secs),
            format_duration_secs(iv)
        ),
        (Some(secs), None) => format!("in {}", format_duration_secs(secs)),
        (None, _) => "—".into(),
    }
}

pub(super) fn format_duration_secs(secs: u32) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}
