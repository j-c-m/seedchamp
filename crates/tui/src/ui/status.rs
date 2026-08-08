//! Process/engine status screen.

use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::Frame;

use crate::app::App;

use super::{
    format_peer_counts, human_bytes, ops_rate_str, peer_counts_global, rate_str,
    render_scrollable_panel,
};

/// Process + engine Status view (seedchamp “top”).
///
/// Uses last `ProcessSample` (TUI-thread sysinfo self-refresh) and existing
/// `SessionSnapshot` only — never opens catalog or takes new engine locks here.
pub(super) fn draw_status_screen(f: &mut Frame, area: Rect, app: &mut App) {
    let th = &app.theme;
    let ps = &app.process_sample;
    let mut lines: Vec<Line> = Vec::with_capacity(48);

    lines.push(Line::from(Span::styled("PROCESS", th.section_style())));

    if !ps.available {
        lines.push(Line::from(format!("  pid {}", ps.pid)));
    } else {
        let uptime = ps
            .uptime_secs
            .map(format_uptime)
            .unwrap_or_else(|| "—".into());
        lines.push(Line::from(format!(
            "  uptime {uptime}   pid {}   version {}",
            ps.pid, app.engine_version
        )));

        let rss = ps.rss_bytes.map(human_bytes).unwrap_or_else(|| "—".into());
        let cpu = ps
            .cpu_pct
            .map(|p| format!("{p:.1}%"))
            .unwrap_or_else(|| "—".into());
        let threads = ps
            .threads
            .map(|n| n.to_string())
            .unwrap_or_else(|| "—".into());
        let fds = match (ps.fd_count, ps.fd_soft_limit) {
            (Some(n), Some(lim)) if lim != u64::MAX => format!("{n} / {lim}"),
            (Some(n), Some(_)) => format!("{n} / unlimited"),
            (Some(n), None) => format!("{n}"),
            _ => "—".into(),
        };
        lines.push(Line::from(format!(
            "  RSS {rss}   CPU {cpu}   threads {threads}   FDs {fds}"
        )));

        // FreeBSD: rusage block ops/s (not bytes). Linux: real B/s.
        if ps.io_as_ops {
            let io_r = ps
                .io_read_bps
                .map(ops_rate_str)
                .unwrap_or_else(|| "—".into());
            let io_w = ps
                .io_write_bps
                .map(ops_rate_str)
                .unwrap_or_else(|| "—".into());
            lines.push(Line::from(format!("  I/O ops  read {io_r}  write {io_w}")));
        } else {
            let io_r = ps.io_read_bps.map(rate_str).unwrap_or_else(|| "—".into());
            let io_w = ps.io_write_bps.map(rate_str).unwrap_or_else(|| "—".into());
            lines.push(Line::from(format!("  proc I/O  read {io_r}  write {io_w}")));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("FILESYSTEMS", th.section_style())));
    if app.filesystems.is_empty() {
        lines.push(Line::from("  —"));
    } else {
        for fs in &app.filesystems {
            let free = human_bytes(fs.free_bytes);
            let total = human_bytes(fs.total_bytes);
            let pct = (fs.used_frac() * 100.0).round() as u32;
            let mut parts: Vec<String> = Vec::new();
            if fs.is_default {
                parts.push("default".into());
            }
            if fs.open_torrents == 1 {
                parts.push("1 torrent".into());
            } else if fs.open_torrents > 1 {
                parts.push(format!("{} torrents", fs.open_torrents));
            }
            let tag = if parts.is_empty() {
                String::new()
            } else {
                format!("  ·  {}", parts.join(" · "))
            };
            let path = if fs.path.len() > 36 {
                format!("…{}", &fs.path[fs.path.len().saturating_sub(35)..])
            } else {
                fs.path.clone()
            };
            lines.push(Line::from(format!(
                "  {path:<36}  {free:>8} / {total:<8}  {pct:>3}% used{tag}"
            )));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("ENGINE", th.section_style())));

    let listen = if app.snap.listen.is_empty() {
        app.listen.to_string()
    } else {
        app.snap.listen.clone()
    };
    let (peers_active, peers_total) = peer_counts_global(app);
    let hot = app.snap.torrents.len();
    lines.push(Line::from(format!(
        "  listen {listen}   peers {}   ↑{}  ↓{}",
        format_peer_counts(peers_active, peers_total),
        rate_str(app.snap.total_upload_bps),
        rate_str(app.snap.total_download_bps),
    )));
    lines.push(Line::from(format!("  hot torrents {hot}")));

    // Config-only disk line — never lock DiskWorker for the UI.
    let disk_backend = app.runtime_template.disk_backend.as_str();
    let disk_depth = app.runtime_template.disk_depth;
    let peer_w = app.peer_workers;
    let hash_w = app.runtime_template.hash_workers.unwrap_or(0);
    let hash_label = if hash_w == 0 {
        "auto".into()
    } else {
        hash_w.to_string()
    };
    lines.push(Line::from(format!(
        "  disk  backend={disk_backend}  depth={disk_depth}   workers peer={peer_w} hash={hash_label}"
    )));
    lines.push(Line::from(format!(
        "  encryption {}   session ↑{} ↓{}",
        app.encryption.as_str(),
        human_bytes(app.snap.total_session_up),
        human_bytes(app.snap.total_session_down),
    )));
    let eng = app.snap.status_line.trim();
    if !eng.is_empty() {
        lines.push(Line::from(format!("  status: {eng}")));
    } else {
        lines.push(Line::from(format!(
            "  status: {}",
            if app.snap.running { "running" } else { "idle" }
        )));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("THREADS", th.section_style())));
    if ps.thread_groups.is_empty() {
        lines.push(Line::from("  —"));
    } else {
        lines.push(Line::from(format!(
            "  {:<22} {:>5} {:>8}",
            "role", "count", "CPU%"
        )));
        for g in &ps.thread_groups {
            let display = if g.name.len() > 22 {
                format!("{}…", &g.name.chars().take(21).collect::<String>())
            } else {
                g.name.clone()
            };
            let cpu = g
                .cpu_pct
                .map(|p| format!("{p:.1}"))
                .unwrap_or_else(|| "—".into());
            lines.push(Line::from(format!(
                "  {display:<22} {:>5} {cpu:>8}",
                g.count
            )));
        }
    }

    render_scrollable_panel(f, area, app, " Status ", lines);
}

pub(super) fn format_uptime(secs: u64) -> String {
    let d = secs / 86_400;
    let h = (secs % 86_400) / 3_600;
    let m = (secs % 3_600) / 60;
    let s = secs % 60;
    if d > 0 {
        format!("{d}d {h:02}:{m:02}:{s:02}")
    } else {
        format!("{h:02}:{m:02}:{s:02}")
    }
}
