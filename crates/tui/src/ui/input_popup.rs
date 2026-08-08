//! Modal input popup (filter, palette, relocate, log filter).

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::widgets::{Clear, Paragraph};
use ratatui::Frame;

use crate::app::{App, Mode};

use super::panel_block;

pub(super) fn draw_input_popup(f: &mut Frame, app: &App) {
    let area = centered_rect(80, 3, f.area());
    f.render_widget(Clear, area);
    let (title, body) = match app.mode {
        Mode::Filter => (" Filter ", format!("/{}", app.input)),
        Mode::LogFilter => (" Log filter ", format!("/{}", app.input)),
        Mode::Palette => (" Command ", format!(":{}", app.input)),
        Mode::Relocate => (" Download path ", app.input.clone()),
        _ => (" Input ", app.input.clone()),
    };
    let p = Paragraph::new(body).block(
        panel_block(title, &app.theme)
            .border_style(app.theme.focus_border())
            .style(Style::default().bg(app.theme.popup_bg)),
    );
    f.render_widget(p, area);
}

pub(super) fn centered_rect(percent_x: u16, height: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - height.min(90)) / 2),
            Constraint::Length(height),
            Constraint::Percentage((100 - height.min(90)) / 2),
        ])
        .split(r);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}
