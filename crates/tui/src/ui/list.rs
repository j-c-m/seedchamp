//! Main torrent list table.

use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Cell, Row, Table};
use ratatui::Frame;

use crate::app::{App, RowUiStatus};

use super::{human_bytes, panel_block, pct_or_done, rate_str, ratio_str, truncate};

pub(super) fn draw_list(f: &mut Frame, area: Rect, app: &mut App) {
    // Fixed columns (everything except NAME). Remaining width goes to NAME.
    // NAME SIZE % ↓ ↑ DN UP RATIO P RUN  — progress column header is blank (self-explanatory).
    // No ID column: catalog ids grow without bound; id still in detail / :commands.
    // RUN is 4 wide so "chk" / "bad" / "err" fit; % is 5 for "done" / "100%".
    // P stays narrow (total peer count only; active/total is footer/detail).
    const COLS: [u16; 9] = [8, 5, 9, 9, 8, 8, 5, 3, 4];
    const NCOLS: u16 = 10; // includes NAME
    let fixed_sum: u16 = COLS.iter().sum();
    // borders (2) + horizontal padding (2)
    let overhead = fixed_sum + (NCOLS - 1) /* spacing */ + 2 + 2;
    let name_w = area.width.saturating_sub(overhead).max(8) as usize;

    // When nothing is selected, treat the header as the focus row (top of list).
    let header_style = if app.selected.is_none() {
        app.theme.selected_row()
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    };
    let header = Row::new(vec![
        "NAME", "SIZE", "", "↓", "↑", "DN", "UP", "RATIO", "P", "RUN",
    ])
    .style(header_style)
    .bottom_margin(0);

    // Build only the visible slice — materializing all 700+ rows every frame
    // was the main idle CPU burn (truss: kevent + TIOCGWINSZ spam).
    let nrows = app.rows.len();
    let view_h = area.height.saturating_sub(3).max(1) as usize; // borders + header
    if let Some(sel) = app.selected {
        let off = app.list_table_state.offset();
        if sel < off {
            *app.list_table_state.offset_mut() = sel;
        } else if sel >= off + view_h {
            *app.list_table_state.offset_mut() = sel + 1 - view_h;
        }
    }
    if nrows > 0 {
        let max_off = nrows.saturating_sub(view_h);
        if app.list_table_state.offset() > max_off {
            *app.list_table_state.offset_mut() = max_off;
        }
    } else {
        *app.list_table_state.offset_mut() = 0;
    }
    let start = app.list_table_state.offset();
    let end = (start + view_h).min(nrows);

    let rows = (start..end).map(|i| {
        let r = &app.rows[i];
        let live = app.live_for(r.id);
        let row_st = app.row_ui.get(&r.id);
        let rechecking = matches!(row_st, Some(RowUiStatus::Rechecking { .. }))
            || r.state.eq_ignore_ascii_case("checking");
        let have = if let Some(RowUiStatus::Rechecking { good, piece_count }) = row_st {
            // Count verified-good pieces 0 → final have % (not scan 0→100%).
            pct_or_done(*good, *piece_count)
        } else if rechecking {
            pct_or_done(0, r.piece_count)
        } else if let Some(l) = live {
            if l.complete {
                "done".into()
            } else {
                pct_or_done(l.have_count, l.piece_count)
            }
        } else if r.complete {
            "done".into()
        } else {
            pct_or_done(r.have_count, r.piece_count)
        };
        let selected = app.selected == Some(i);
        // run_off is muted; selected_muted stays readable on selected row.
        let th = &app.theme;
        let catalog_err = r.error_msg.as_ref().is_some_and(|e| !e.is_empty());
        let (run_txt, run_fg) = match row_st {
            Some(st @ RowUiStatus::Rechecking { .. }) => (st.run_label(), th.run_check),
            Some(st @ RowUiStatus::RecheckDone { complete: true, .. }) => (st.run_label(), th.ok),
            Some(
                st @ RowUiStatus::RecheckDone {
                    complete: false, ..
                },
            ) => (st.run_label(), th.run_err),
            Some(st @ RowUiStatus::RecheckFailed { .. }) => (st.run_label(), th.run_err),
            None if rechecking => ("chk", th.run_check),
            None if r.want_start => ("on", th.run_on),
            // Sticky catalog error (e.g. startup storage demote); full text in detail.
            None if catalog_err => ("err", th.run_err),
            None if selected => ("off", th.selected_muted),
            None => ("off", th.run_off),
        };
        // P = connected peer count only (compact); active/total is footer/detail.
        let peers = live
            .map(|l| l.peer_count.to_string())
            .unwrap_or_else(|| "·".into());
        let down_bps = live.map(|l| l.download_bps).unwrap_or(0);
        let up_bps = live.map(|l| l.upload_bps).unwrap_or(0);
        let down_rate = if down_bps > 0 {
            rate_str(down_bps)
        } else {
            "—".into()
        };
        let up_rate = if up_bps > 0 {
            rate_str(up_bps)
        } else {
            "—".into()
        };
        // Lifetime UP: live counter is catalog-seeded + this process (not max(session_delta)).
        let uploaded = live
            .map(|l| r.uploaded.max(l.lifetime_uploaded))
            .unwrap_or(r.uploaded);
        // DN column = lifetime (catalog), not less than verified have while running.
        let downloaded = live
            .map(|l| r.downloaded.max(l.completed_bytes))
            .unwrap_or(r.downloaded);
        let is_complete = live.map(|l| l.complete).unwrap_or(r.complete);

        // Whole-row foreground by activity (seed idle = normal fg).
        // Priority: error → recheck → leech → active seed → idle seed → stopped.
        let row_fg = if catalog_err
            || matches!(
                row_st,
                Some(RowUiStatus::RecheckFailed { .. })
                    | Some(RowUiStatus::RecheckDone {
                        complete: false,
                        ..
                    })
            ) {
            Some(th.run_err)
        } else if rechecking || matches!(row_st, Some(RowUiStatus::Rechecking { .. })) {
            Some(th.recheck_row)
        } else if r.want_start && !is_complete {
            // Leeching (incomplete + started).
            Some(th.progress_partial)
        } else if r.want_start && is_complete && up_bps > 0 {
            // Active seeding.
            Some(th.ok)
        } else if r.want_start && is_complete {
            // Idle seed — default foreground (selected keeps selected_fg).
            None
        } else if !r.want_start {
            Some(if selected {
                th.selected_muted
            } else {
                th.muted
            })
        } else {
            None
        };

        let mut style = if selected {
            th.selected_row()
        } else {
            Style::default()
        };
        if let Some(fg) = row_fg {
            style = style.fg(fg);
        }
        let mut run_style = Style::default().fg(run_fg).add_modifier(Modifier::BOLD);
        if selected {
            run_style = run_style.bg(th.selected_bg).fg(run_fg);
        }
        Row::new(vec![
            Cell::from(truncate(&r.name, name_w)),
            Cell::from(human_bytes(r.total_size)),
            Cell::from(have),
            Cell::from(down_rate),
            Cell::from(up_rate),
            Cell::from(human_bytes(downloaded)),
            Cell::from(human_bytes(uploaded)),
            Cell::from(ratio_str(uploaded, downloaded)),
            Cell::from(peers),
            Cell::from(run_txt).style(run_style),
        ])
        .style(style)
    });

    let table = Table::new(
        rows,
        [
            Constraint::Min(name_w as u16), // fills remaining terminal width
            Constraint::Length(COLS[0]),
            Constraint::Length(COLS[1]),
            Constraint::Length(COLS[2]),
            Constraint::Length(COLS[3]),
            Constraint::Length(COLS[4]),
            Constraint::Length(COLS[5]),
            Constraint::Length(COLS[6]),
            Constraint::Length(COLS[7]),
            Constraint::Length(COLS[8]),
        ],
    )
    .header(header)
    .block(panel_block(
        format!(
            " Torrents  {}  sort:{} ",
            app.rows.len(),
            app.list_sort.label()
        ),
        &app.theme,
    ))
    .column_spacing(1)
    .row_highlight_style(app.theme.selected_row());
    // No highlight_symbol: a leading "›" shifts columns when selection
    // moves between header (no row selected) and a torrent row.

    // View-local state: we already sliced by absolute offset; select relative index.
    let mut view_state = ratatui::widgets::TableState::default();
    if let Some(sel) = app.selected {
        if sel >= start && sel < end {
            view_state.select(Some(sel - start));
        }
    }
    f.render_stateful_widget(table, area, &mut view_state);
}
