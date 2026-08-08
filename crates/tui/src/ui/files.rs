//! Per-torrent files tree table.

use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Cell, Paragraph, Row, Table};
use ratatui::Frame;

use super::{human_bytes, pad_right, panel_block, truncate};
use crate::app::App;

pub(super) fn draw_files(f: &mut Frame, area: Rect, app: &mut App) {
    use crate::file_tree::{aggregate_pct, DirWanted, FileTreeRow};

    let tid = app.files_torrent_id;
    let tname = tid
        .and_then(|id| app.rows.iter().find(|r| r.id == id).map(|r| r.name.clone()))
        .unwrap_or_else(|| "—".into());
    let n_files = app.files.len();
    let n_dirs = app
        .file_tree
        .iter()
        .filter(|r| matches!(r, FileTreeRow::Dir { .. }))
        .count();
    let title = if let Some(id) = tid {
        format!(
            " Files  #{id} {}  · {n_files} file(s) {n_dirs} dir(s) ",
            truncate(&tname, 28),
        )
    } else {
        " Files ".into()
    };

    if app.file_tree.is_empty() {
        f.render_widget(
            Paragraph::new("—")
                .style(app.theme.muted_style())
                .block(panel_block(title, &app.theme)),
            area,
        );
        return;
    }

    const W_ON: usize = 3;
    const W_PCT: usize = 5;
    const W_IDX: usize = 4;
    const W_SIZE: usize = 10;
    // spacing (4 cols + name gaps) + borders (2) + horizontal padding (2)
    let name_w = area
        .width
        .saturating_sub((W_ON + W_PCT + W_IDX + W_SIZE) as u16 + 6 + 2 + 2)
        .max(8) as usize;

    let header = Row::new(vec![
        Cell::from("ON"),
        Cell::from(""), // %
        Cell::from("IDX"),
        Cell::from(pad_right("SIZE", W_SIZE)),
        Cell::from("NAME"),
    ])
    .style(Style::default().add_modifier(Modifier::BOLD));

    let rows = app.file_tree.iter().enumerate().map(|(i, row)| {
        let base = if i == app.file_selected {
            app.theme.selected_row()
        } else {
            Style::default()
        };
        match row {
            FileTreeRow::Dir {
                name,
                depth,
                expanded,
                size,
                have_bytes,
                wanted,
                ..
            } => {
                let th = &app.theme;
                let (on, on_fg) = match wanted {
                    DirWanted::All => ("on", th.file_on),
                    DirWanted::None => ("off", th.file_off),
                    DirWanted::Mixed => ("mix", th.file_mix),
                };
                let pct_n = aggregate_pct(*have_bytes, *size);
                let (pct, pct_fg) = if pct_n >= 100 {
                    ("done".to_string(), th.progress_done)
                } else if pct_n > 0 {
                    (format!("{pct_n}%"), th.progress_partial)
                } else {
                    ("0%".into(), th.progress_empty)
                };
                let mark = if *expanded { "▾" } else { "▸" };
                let indent = "  ".repeat(*depth);
                let label = format!("{indent}{mark} {name}/");
                let on_style = Style::default().fg(on_fg).add_modifier(Modifier::BOLD);
                let pct_style = Style::default().fg(pct_fg);
                let name_style = if matches!(wanted, DirWanted::None) {
                    th.muted_style()
                } else {
                    th.accent_style()
                };
                Row::new(vec![
                    Cell::from(on).style(on_style),
                    Cell::from(pad_right(&pct, W_PCT)).style(pct_style),
                    Cell::from("  ·"),
                    Cell::from(pad_right(&human_bytes(*size), W_SIZE)),
                    Cell::from(truncate(&label, name_w)).style(name_style),
                ])
                .style(base)
            }
            FileTreeRow::File { depth, file_index } => {
                let fp = &app.files[*file_index];
                let file = &fp.file;
                let th = &app.theme;
                let on = if fp.wanted() { "on" } else { "off" };
                let on_style = if fp.wanted() {
                    Style::default().fg(th.file_on).add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                        .fg(th.file_off)
                        .add_modifier(Modifier::BOLD)
                };
                let pct = if fp.done() {
                    "done".to_string()
                } else {
                    format!("{}%", fp.pct())
                };
                let pct_style = if fp.done() {
                    Style::default().fg(th.progress_done)
                } else if fp.pct() > 0 {
                    Style::default().fg(th.progress_partial)
                } else {
                    Style::default().fg(th.progress_empty)
                };
                let base_name = std::path::Path::new(&file.path)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or(file.path.as_str());
                let indent = "  ".repeat(*depth);
                let label = format!("{indent}  {base_name}");
                Row::new(vec![
                    Cell::from(on).style(on_style),
                    Cell::from(pad_right(&pct, W_PCT)).style(pct_style),
                    Cell::from(format!("{:>3}", file.idx)),
                    Cell::from(pad_right(&human_bytes(file.size), W_SIZE)),
                    Cell::from(truncate(&label, name_w)),
                ])
                .style(base)
            }
        }
    });

    let table = Table::new(
        rows,
        [
            Constraint::Length(W_ON as u16),
            Constraint::Length(W_PCT as u16),
            Constraint::Length(W_IDX as u16),
            Constraint::Length(W_SIZE as u16),
            Constraint::Min(name_w as u16),
        ],
    )
    .header(header)
    .block(panel_block(title, &app.theme))
    .column_spacing(1)
    .row_highlight_style(app.theme.selected_row());
    f.render_stateful_widget(table, area, &mut app.file_table_state);
}
