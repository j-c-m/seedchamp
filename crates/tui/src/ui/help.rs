//! Help and log-help screens.

use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::Frame;

use crate::app::App;

use super::render_scrollable_panel;

pub(super) fn draw_log_help(f: &mut Frame, area: Rect, app: &mut App) {
    let cap = app.activity.capture_filter();
    let view = if app.log_filter.trim().is_empty() {
        "all".to_string()
    } else {
        format!("{:?}", app.log_filter.trim())
    };
    let th = &app.theme;
    let text = vec![
        Line::from(Span::styled("Keys", th.section_style())),
        Line::from("  j/k  ↑↓         scroll"),
        Line::from("  PgUp / PgDn     page"),
        Line::from("  g / Home        oldest"),
        Line::from("  G / End         follow newest"),
        Line::from("  l / Esc / q     back"),
        Line::from("  ? / h           this help"),
        Line::from(""),
        Line::from(Span::styled("Display filter", th.section_style())),
        Line::from("  /               edit filter"),
        Line::from("  c               clear filter"),
        Line::from("  substring       match any field"),
        Line::from("  e  w  i  d  t   level only"),
        Line::from("  >=w             min level"),
        Line::from(format!("  current         {view}")),
        Line::from(""),
        Line::from(Span::styled("Capture level", th.section_style())),
        Line::from("  v               cycle capture level"),
        Line::from("  :log debug      set capture and open log"),
        Line::from("  :loglevel info  set capture only"),
        Line::from(format!("  current         {cap}")),
    ];
    render_scrollable_panel(f, area, app, " Log help ", text);
}

pub(super) fn draw_help(f: &mut Frame, area: Rect, app: &mut App) {
    let th = &app.theme;
    let text = vec![
        Line::from(Span::styled("Keys", th.section_style())),
        Line::from("  j/k ↑↓     move selection"),
        Line::from("  Space      clear selection"),
        Line::from("  Enter      torrent detail"),
        Line::from("  l          activity log"),
        Line::from("  1 / 2      list screen;  o  cycle sort"),
        Line::from("  s          status"),
        Line::from("  C-s        start/stop selected"),
        Line::from("  p          peers"),
        Line::from("  f          files"),
        Line::from("  C-r        recheck selected"),
        Line::from("  C-d        soft-delete selected"),
        Line::from("  C-o        change download path"),
        Line::from("  /          filter    :  commands    ?  help"),
        Line::from("  C-q        quit    :quit"),
        Line::from(""),
        Line::from(Span::styled("List sort", th.section_style())),
        Line::from("  1 rate   off↑ · ↓rate · ↑rate · added · name"),
        Line::from("  2 name   name A–Z"),
        Line::from("  1/2 jump · o or :sort cycle · :sort 1|2|rate|name"),
        Line::from(""),
        Line::from(Span::styled("Files", th.section_style())),
        Line::from("  j/k        select"),
        Line::from("  Space      on/off    Enter expand dir"),
        Line::from(""),
        Line::from(Span::styled("Activity log", th.section_style())),
        Line::from("  / filter · v capture level · ? log help"),
        Line::from(""),
        Line::from(Span::styled("Commands (:)", th.section_style())),
        Line::from("  :start [id]  :stop [id]  :recheck [id]  :peers  :files"),
        Line::from("  :sort 1|2|rate|name   :add <path|url> [start] [data=DIR]"),
        Line::from("  :remove [id] :filter text :limits up=10m down=0"),
        Line::from("  :log [level]   :loglevel debug|info|…   :quit"),
        Line::from(""),
        Line::from(Span::styled("Theme", th.section_style())),
        Line::from(format!("  active       {}", th.name)),
        Line::from("  default      ANSI stock"),
        Line::from("  soft         truecolor"),
        Line::from("  file         themes/<name>.toml"),
    ];
    render_scrollable_panel(f, area, app, " Help ", text);
}
