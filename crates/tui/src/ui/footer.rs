//! Status bar and footer key hints.

use ratatui::layout::Rect;
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::app::{App, Mode};

use super::{format_peer_counts, human_bytes, peer_counts_global, rate_str};

pub(super) fn draw_status(f: &mut Frame, area: Rect, app: &App) {
    // One-col indent so text is not flush against the terminal edge.
    let text = if app.status.is_empty() {
        String::new()
    } else {
        format!(" {}", app.status)
    };
    let p = Paragraph::new(text).style(app.theme.status_line());
    f.render_widget(p, area);
}

pub(super) fn draw_footer(f: &mut Frame, area: Rect, app: &App) {
    let mode = match app.mode {
        Mode::List => "LIST",
        Mode::Detail => "DETAIL",
        Mode::Peers => "PEERS",
        Mode::Files => "FILES",
        Mode::Log => "LOG",
        Mode::LogHelp => "LOG?",
        Mode::LogFilter => "LOG/",
        Mode::Filter => "FILTER",
        Mode::Palette => "CMD",
        Mode::Relocate => "PATH",
        Mode::Help => "HELP",
        Mode::Status => "STATUS",
    };
    // Catalog lifetime totals across listed torrents, blended with live session.
    let (tot_up, tot_dn) = list_totals(app);
    let (peers_active, peers_total) = peer_counts_global(app);
    let text = format!(
        " {mode} │ sort:{} │ ctrl→{} io │ peers={} │ ↓{} ↑{} │ DN {} UP {} ",
        app.list_sort.label(),
        app.peer_workers,
        format_peer_counts(peers_active, peers_total),
        rate_str(app.snap.total_download_bps),
        rate_str(app.snap.total_upload_bps),
        human_bytes(tot_dn),
        human_bytes(tot_up),
    );
    f.render_widget(Paragraph::new(text).style(app.theme.footer_bar()), area);
}

/// Sum DN/UP across catalog rows, using live totals when higher.
pub(super) fn list_totals(app: &App) -> (u64, u64) {
    let mut up = 0u64;
    let mut dn = 0u64;
    for r in &app.rows {
        let live = app.live_for(r.id);
        up += live
            .map(|l| r.uploaded.max(l.lifetime_uploaded))
            .unwrap_or(r.uploaded);
        // Lifetime DN: catalog, not less than verified have while running.
        dn += live
            .map(|l| r.downloaded.max(l.completed_bytes))
            .unwrap_or(r.downloaded);
    }
    (up, dn)
}
