//! Peers table screen.

use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Cell, Paragraph, Row, Table};
use ratatui::Frame;

use crate::app::App;

use super::{
    format_peer_counts, human_bytes, pad_right, panel_block, peer_is_active, rate_str, truncate,
};

pub(super) fn draw_peers(f: &mut Frame, area: Rect, app: &mut App) {
    // Scope peers to the selected torrent when set.
    let sel_id = app.selected_id();
    let peers = app.peers_for_screen();
    let (scope_id, torrent_name) = if let Some(id) = sel_id {
        let name = app
            .rows
            .iter()
            .find(|r| r.id == id)
            .map(|r| r.name.clone())
            .or_else(|| {
                app.snap
                    .peers
                    .iter()
                    .find(|p| p.torrent_id == id)
                    .map(|p| p.torrent_name.clone())
            })
            .unwrap_or_else(|| format!("#{id}"));
        (Some(id), name)
    } else {
        (None, "all torrents".into())
    };

    // Fixed column char widths. Numeric cols pad-right so units line up.
    // Global (all-torrents) view adds a NAME column; single-torrent omits it.
    // D column is direction+crypto: i-/ip/i4/i? and o-/op/o4/o?
    let show_name = scope_id.is_none();
    const W_DIR: usize = 2;
    const W_ADDR: usize = 22;
    const W_NAME: usize = 14;
    const W_CLIENT: usize = 18;
    const W_PCT: usize = 5;
    const W_RATE: usize = 10;
    // Dual-role: e.g. "12/64·unc·↑3" or "0/4·chk·ni"
    const W_QUEUE: usize = 13;
    const W_BYTES: usize = 9;
    const W_AGE: usize = 4;

    let mut header_cells = vec![Cell::from("D"), Cell::from("ADDR")];
    if show_name {
        header_cells.push(Cell::from("NAME"));
    }
    header_cells.extend([
        Cell::from("CLIENT"),
        Cell::from(pad_right("%", W_PCT)),
        Cell::from(pad_right("↓", W_RATE)),
        Cell::from(pad_right("↑", W_RATE)),
        Cell::from(pad_right("QUEUE", W_QUEUE)),
        Cell::from(pad_right("DN", W_BYTES)),
        Cell::from(pad_right("UP", W_BYTES)),
        Cell::from(pad_right("AGE", W_AGE)),
    ]);
    let header = Row::new(header_cells).style(Style::default().add_modifier(Modifier::BOLD));

    let n = peers.len();
    let n_active = peers.iter().filter(|p| peer_is_active(p)).count();
    let peer_label = format_peer_counts(n_active, n);
    let title = if let Some(id) = scope_id {
        format!(
            " Peers  #{id} {}  · {peer_label} ",
            truncate(&torrent_name, 36)
        )
    } else {
        format!(" Peers  all  · {peer_label} ")
    };

    if n == 0 {
        f.render_widget(
            Paragraph::new("—")
                .style(app.theme.muted_style())
                .block(panel_block(title, &app.theme)),
            area,
        );
        return;
    }

    // Keep selection valid if peer dropped; pin scroll to selection.
    let sel_idx = app
        .peer_selected_id
        .and_then(|id| peers.iter().position(|p| p.id == id));
    let sel_idx = match sel_idx {
        Some(i) => i,
        None => {
            // Lost peer or first open mid-session — pick nearest to scroll.
            let i = app.peer_scroll.min(n - 1);
            app.peer_selected_id = Some(peers[i].id);
            i
        }
    };
    let view_h = area.height.saturating_sub(3).max(1) as usize; // borders + header
    if sel_idx < app.peer_scroll {
        app.peer_scroll = sel_idx;
    } else if sel_idx >= app.peer_scroll + view_h {
        app.peer_scroll = sel_idx + 1 - view_h;
    }
    let max_off = n.saturating_sub(view_h);
    if app.peer_scroll > max_off {
        app.peer_scroll = max_off;
    }
    let start = app.peer_scroll;
    let end = (start + view_h).min(n);

    let rows = (start..end).map(|i| {
        let p = &peers[i];
        let d = peer_dir_crypto(p);
        let queue = format_peer_queue(p);
        let pct = peer_complete_pct(p);
        let age = if p.connected_secs >= 3600 {
            format!("{}h", p.connected_secs / 3600)
        } else if p.connected_secs >= 60 {
            format!("{}m", p.connected_secs / 60)
        } else {
            format!("{}s", p.connected_secs)
        };
        let addr = truncate(&p.addr.to_string(), W_ADDR);
        let client = if p.client.is_empty() {
            "…".into()
        } else {
            truncate(&p.client, W_CLIENT)
        };
        let mut cells = vec![Cell::from(d), Cell::from(addr)];
        if show_name {
            let name = if p.torrent_id == 0 {
                "…".into()
            } else if p.torrent_name.is_empty() {
                format!("#{}", p.torrent_id)
            } else {
                truncate(&p.torrent_name, W_NAME)
            };
            cells.push(Cell::from(name));
        }
        cells.extend([
            Cell::from(client),
            Cell::from(pad_right(&pct, W_PCT)),
            Cell::from(pad_right(&rate_str(p.download_bps), W_RATE)),
            Cell::from(pad_right(&rate_str(p.upload_bps), W_RATE)),
            Cell::from(pad_right(&queue, W_QUEUE)),
            Cell::from(pad_right(&human_bytes(p.downloaded), W_BYTES)),
            Cell::from(pad_right(&human_bytes(p.uploaded), W_BYTES)),
            Cell::from(pad_right(&age, W_AGE)),
        ]);
        let selected = app.peer_selected_id == Some(p.id);
        let style = if selected {
            app.theme.selected_row()
        } else {
            Style::default()
        };
        Row::new(cells).style(style)
    });

    let mut constraints = vec![
        Constraint::Length(W_DIR as u16),
        Constraint::Length(W_ADDR as u16),
    ];
    if show_name {
        constraints.push(Constraint::Length(W_NAME as u16));
    }
    constraints.extend([
        Constraint::Length(W_CLIENT as u16),
        Constraint::Length(W_PCT as u16),
        Constraint::Length(W_RATE as u16),
        Constraint::Length(W_RATE as u16),
        Constraint::Length(W_QUEUE as u16),
        Constraint::Length(W_BYTES as u16),
        Constraint::Length(W_BYTES as u16),
        Constraint::Length(W_AGE as u16),
    ]);

    let table = Table::new(rows, constraints)
        .header(header)
        .block(panel_block(title, &app.theme))
        .column_spacing(1)
        .row_highlight_style(app.theme.selected_row());

    let mut view_state = ratatui::widgets::TableState::default();
    if sel_idx >= start && sel_idx < end {
        view_state.select(Some(sel_idx - start));
    }
    f.render_stateful_widget(table, area, &mut view_state);
}
pub(super) fn peer_complete_pct(p: &seedchamp_engine::PeerInfo) -> String {
    if p.piece_count == 0 {
        return "—".into();
    }
    if p.peer_have >= p.piece_count {
        return "done".into();
    }
    let pct = (100u64 * p.peer_have as u64 / p.piece_count as u64) as u32;
    format!("{pct}%")
}

/// Role-aware peer QUEUE / state cell for seed-while-leech diagnosis.
///
/// Parts joined with `·` (truncate to column width):
/// - **Download:** `out/tgt` if leech pipe armed (`queue_target > 0`), else
///   `i` if we are Interested without a published target.
/// - **Choke (download):** `unc` = they unchoked us; `chk` = choking us
///   (only shown when we care about download: pipe or am_interested).
/// - **Upload:** `↑N` / `↑` = they are Interested (+ pending Requests);
///   `ni` = NotInterested (or never Interested).
///
/// Examples: `456/478·unc·ni`  `0/4·chk·↑`  `↑12`  `ni`
pub(super) fn format_peer_queue(p: &seedchamp_engine::PeerInfo) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(3);

    let leech_side = p.queue_target > 0 || p.am_interested;
    if p.queue_target > 0 {
        parts.push(format!("{}/{}", p.queue_outstanding, p.queue_target));
    } else if p.am_interested {
        parts.push("i".into());
    }

    if leech_side {
        parts.push(if p.peer_choking {
            "chk".into()
        } else {
            "unc".into()
        });
    }

    if p.peer_interested {
        if p.upload_pending > 0 {
            parts.push(format!("↑{}", p.upload_pending));
        } else {
            parts.push("↑".into());
        }
    } else if p.upload_pending > 0 {
        // Race: still serving after NotInterested.
        parts.push(format!("↑{}", p.upload_pending));
    } else {
        parts.push("ni".into());
    }

    parts.join("·")
}

pub(super) fn peer_dir_crypto(p: &seedchamp_engine::PeerInfo) -> String {
    let d = match p.direction {
        seedchamp_engine::PeerDirection::Inbound => 'i',
        seedchamp_engine::PeerDirection::Outbound => 'o',
    };
    format!("{d}{}", p.crypto.wire_tag())
}
