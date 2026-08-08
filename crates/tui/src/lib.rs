//! seedchamp TUI — list, detail, command palette (Phase 6).

#![forbid(unsafe_code)]

mod app;
mod file_tree;
mod helpers;
mod path_complete;
mod sort;
mod theme;
mod ui;

use std::io;
use std::path::Path;
use std::time::Duration;

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::path::PathBuf;

use seedchamp_engine::{
    Result, RuntimeConfig, SessionLimits, WatchConfig, VERSION as ENGINE_VERSION,
};

use app::{App, Mode, PaletteAction};

pub use app::{ListSort, ListSortScreen, SortCriterion};
pub use theme::Theme;

/// Interactive TUI with config-derived runtime settings and list sort.
pub fn run_with_settings_full_sort(
    db: &Path,
    runtime: RuntimeConfig,
    limits: SessionLimits,
    watch: WatchConfig,
    data_root: PathBuf,
    torrent_dir: PathBuf,
    leech_cache: PathBuf,
    leech_cache_size: u64,
    list_sort: ListSort,
    theme: Theme,
) -> Result<()> {
    // Fallback: non-TTY → print table (scripts / pipes).
    use std::io::IsTerminal;
    if !io::stdout().is_terminal() {
        return run_plain_list(db);
    }

    // Build app / start swarm *before* alternate screen so any rare delay is visible,
    // and swarm bootstrap itself must not block (see SessionRuntime::start).
    let mut app = App::new_with_runtime_and_watch_sort(
        db,
        runtime,
        limits,
        watch,
        data_root,
        torrent_dir,
        leech_cache,
        leech_cache_size,
        list_sort,
        theme,
    )?;

    enable_raw_mode().map_err(|e| seedchamp_engine::Error::Msg(format!("tui raw mode: {e}")))?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)
        .map_err(|e| seedchamp_engine::Error::Msg(format!("tui enter: {e}")))?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)
        .map_err(|e| seedchamp_engine::Error::Msg(format!("tui terminal: {e}")))?;

    let res = run_loop(&mut terminal, &mut app);
    // Paint quit-time stopped-announce progress instead of freezing on a
    // blocking wait after leaving the input loop.
    let shut = run_shutdown_ui(&mut terminal, &mut app);

    disable_raw_mode().ok();
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )
    .ok();
    terminal.show_cursor().ok();

    res.and(shut)
}

fn run_loop(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, app: &mut App) -> Result<()> {
    // Only redraw when something changed — idle was still TIOCGWINSZ+write every poll.
    let mut dirty = true;
    loop {
        if dirty {
            terminal
                .draw(|f| ui::draw(f, app))
                .map_err(|e| seedchamp_engine::Error::Msg(format!("tui draw: {e}")))?;
            dirty = false;
        }

        // Max wait for input; returns immediately when a key is ready.
        // 1s matches snapshot refresh; log screen polls faster for live lines.
        let poll_ms = if matches!(app.mode, Mode::Log | Mode::LogFilter | Mode::LogHelp) {
            250u64
        } else {
            1000u64
        };
        if event::poll(Duration::from_millis(poll_ms))
            .map_err(|e| seedchamp_engine::Error::Msg(format!("tui poll: {e}")))?
        {
            match event::read()
                .map_err(|e| seedchamp_engine::Error::Msg(format!("tui read: {e}")))?
            {
                Event::Key(key) => {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    if handle_key(app, key)? {
                        app.begin_shutdown();
                        break;
                    }
                    dirty = true;
                }
                Event::Mouse(m) => {
                    if handle_mouse(app, m) {
                        dirty = true;
                    }
                }
                // Resize / other events — redraw.
                _ => dirty = true,
            }
        }
        // Snapshot + rates; returns true if UI data changed.
        if app.tick_refresh()? {
            dirty = true;
        }
        // Live log: pull new tracing lines while log (or filter prompt) is open.
        if matches!(app.mode, Mode::Log | Mode::LogFilter | Mode::LogHelp)
            && app.poll_activity_log()
        {
            dirty = true;
        }
    }
    Ok(())
}

/// Keep the alternate screen alive while quit-time `event=stopped` runs.
///
/// Status line shows `quitting — stopped announce N/M…` (and retry lines) from
/// the engine snapshot until the control session is torn down.
fn run_shutdown_ui(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> Result<()> {
    if !app.quitting {
        app.begin_shutdown();
    }
    // Immediate first paint so C-q is not a frozen last frame.
    terminal
        .draw(|f| ui::draw(f, app))
        .map_err(|e| seedchamp_engine::Error::Msg(format!("tui draw (quit): {e}")))?;

    loop {
        let done = app.poll_shutdown();
        terminal
            .draw(|f| ui::draw(f, app))
            .map_err(|e| seedchamp_engine::Error::Msg(format!("tui draw (quit): {e}")))?;
        if done {
            break;
        }
        // Drain input so a stuck key does not pile up; resize still redraws next pass.
        if event::poll(Duration::from_millis(200))
            .map_err(|e| seedchamp_engine::Error::Msg(format!("tui poll (quit): {e}")))?
        {
            let _ = event::read();
            while event::poll(Duration::from_millis(0)).unwrap_or(false) {
                let _ = event::read();
            }
        }
    }

    app.finish_shutdown();
    Ok(())
}

/// Returns true if the app should quit.
fn handle_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    let code = key.code;
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    // Ctrl chords (rtorrent-style) — active outside text entry.
    if ctrl
        && !matches!(
            app.mode,
            Mode::Filter | Mode::LogFilter | Mode::Palette | Mode::Relocate
        )
    {
        match code {
            // Quit from any non-text mode (list used bare `q` too easily by accident).
            KeyCode::Char('q') | KeyCode::Char('Q') => return Ok(true),
            KeyCode::Char('s') | KeyCode::Char('S') => {
                app.toggle_start_selected()?;
                return Ok(false);
            }
            KeyCode::Char('r') | KeyCode::Char('R') => {
                app.recheck_selected()?;
                return Ok(false);
            }
            KeyCode::Char('o') | KeyCode::Char('O') => {
                app.begin_relocate()?;
                return Ok(false);
            }
            KeyCode::Char('d') | KeyCode::Char('D') => {
                app.delete_selected()?;
                return Ok(false);
            }
            _ => {}
        }
    }

    match app.mode {
        Mode::List => match code {
            // Quit only via C-q / :quit (bare q and Esc do not quit the list).
            KeyCode::Char('j') | KeyCode::Down => app.select_next(),
            KeyCode::Char('k') | KeyCode::Up => app.select_prev(),
            KeyCode::PageDown => app.select_page(1),
            KeyCode::PageUp => app.select_page(-1),
            KeyCode::Home | KeyCode::Char('g') => app.select_first(),
            KeyCode::End | KeyCode::Char('G') => app.select_last(),
            // Esc on list: clear selection (top of list, no highlight).
            KeyCode::Char(' ') if app.selected.is_some() => app.clear_selection(),
            KeyCode::Enter => app.open_detail()?,
            KeyCode::Char('l') => app.open_log(),
            KeyCode::Char('p') => app.open_peers(),
            KeyCode::Char('f') => app.open_files()?,
            KeyCode::Char('/') => {
                app.mode = Mode::Filter;
                app.input.clear();
            }
            KeyCode::Char(':') => {
                app.mode = Mode::Palette;
                app.input.clear();
            }
            KeyCode::Char('?') | KeyCode::Char('h') => app.open_help(),
            // bare s → Status; start/stop is Ctrl-s only (global chord above).
            KeyCode::Char('s') => app.open_status(),
            // recheck is Ctrl-r only (global chord above)
            KeyCode::Char('o') => app.cycle_list_sort(),
            // Number keys jump to named list screens (1=rate, 2=name).
            KeyCode::Char('1') => app.set_list_sort("1"),
            KeyCode::Char('2') => app.set_list_sort("2"),
            _ => {}
        },
        Mode::Detail => match code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Backspace => {
                app.mode = Mode::List;
                app.detail = None;
            }
            KeyCode::Char('j') | KeyCode::Down => app.detail_scroll(1),
            KeyCode::Char('k') | KeyCode::Up => app.detail_scroll(-1),
            KeyCode::PageDown => app.detail_scroll_page(1),
            KeyCode::PageUp => app.detail_scroll_page(-1),
            KeyCode::Home | KeyCode::Char('g') => app.detail_scroll_home(),
            KeyCode::End | KeyCode::Char('G') => app.detail_scroll_end(),
            KeyCode::Char('p') => app.open_peers(),
            KeyCode::Char('f') => app.open_files()?,
            KeyCode::Char('l') => app.open_log(),
            KeyCode::Char('s') => app.open_status(),
            KeyCode::Char(':') => {
                app.mode = Mode::Palette;
                app.input.clear();
            }
            KeyCode::Char('?') => app.open_help(),
            _ => {}
        },
        Mode::Peers => match code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('p') => {
                app.mode = Mode::List;
            }
            KeyCode::Char('l') => app.open_log(),
            KeyCode::Char('j') | KeyCode::Down => app.peer_select_delta(1),
            KeyCode::Char('k') | KeyCode::Up => app.peer_select_delta(-1),
            KeyCode::PageDown => app.peer_select_delta(10),
            KeyCode::PageUp => app.peer_select_delta(-10),
            KeyCode::Home | KeyCode::Char('g') => app.peer_select_first(),
            KeyCode::End | KeyCode::Char('G') => app.peer_select_last(),
            KeyCode::Char('s') => app.open_status(),
            KeyCode::Char('f') => app.open_files()?,
            _ => {}
        },
        Mode::Files => match code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('f') => {
                app.close_files();
            }
            KeyCode::Char('j') | KeyCode::Down => app.file_select_delta(1),
            KeyCode::Char('k') | KeyCode::Up => app.file_select_delta(-1),
            KeyCode::PageDown => app.file_select_delta(10),
            KeyCode::PageUp => app.file_select_delta(-10),
            KeyCode::Home | KeyCode::Char('g') => app.file_select_first(),
            KeyCode::End | KeyCode::Char('G') => app.file_select_last(),
            // Expand/collapse directories; Space toggles on/off (file or whole dir).
            KeyCode::Enter | KeyCode::Char('h') | KeyCode::Left | KeyCode::Right => {
                app.toggle_file_dir_expand()
            }
            KeyCode::Char(' ') => app.toggle_file_selected()?,
            KeyCode::Char('p') => app.open_peers(),
            KeyCode::Char('l') => app.open_log(),
            KeyCode::Char('s') => app.open_status(),
            _ => {}
        },
        Mode::Status => match code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('s') => {
                app.mode = Mode::List;
                app.status.clear();
            }
            KeyCode::Char('j') | KeyCode::Down => app.pane_scroll(1),
            KeyCode::Char('k') | KeyCode::Up => app.pane_scroll(-1),
            KeyCode::PageDown => app.pane_scroll_page(1),
            KeyCode::PageUp => app.pane_scroll_page(-1),
            KeyCode::Home | KeyCode::Char('g') => app.pane_scroll_home(),
            KeyCode::End | KeyCode::Char('G') => app.pane_scroll_end(),
            KeyCode::Char('p') => app.open_peers(),
            KeyCode::Char('l') => app.open_log(),
            KeyCode::Char('?') | KeyCode::Char('h') => app.open_help(),
            _ => {}
        },
        Mode::Log => match code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('l') => app.close_log(),
            KeyCode::Char('j') | KeyCode::Down => app.log_scroll(-1),
            KeyCode::Char('k') | KeyCode::Up => app.log_scroll(1),
            // Page ≈ viewport height minus a couple rows of context.
            KeyCode::PageDown => {
                let d = app.log_page_delta();
                app.log_scroll(-d);
            }
            KeyCode::PageUp => {
                let d = app.log_page_delta();
                app.log_scroll(d);
            }
            KeyCode::Home | KeyCode::Char('g') => app.log_scroll_home(),
            KeyCode::End | KeyCode::Char('G') => app.log_scroll_end(),
            KeyCode::Char('/') => app.begin_log_filter(),
            KeyCode::Char('c') => app.clear_log_filter(),
            // Cycle capture verbosity (what enters the ring), not display filter.
            KeyCode::Char('v') => app.cycle_log_capture(),
            KeyCode::Char('?') | KeyCode::Char('h') => app.open_log_help(),
            _ => {}
        },
        Mode::LogHelp => match code {
            KeyCode::Esc
            | KeyCode::Char('q')
            | KeyCode::Char('?')
            | KeyCode::Char('h')
            | KeyCode::Char('l') => {
                app.mode = Mode::Log;
                app.status = app.log_status_line();
            }
            KeyCode::Char('j') | KeyCode::Down => app.pane_scroll(1),
            KeyCode::Char('k') | KeyCode::Up => app.pane_scroll(-1),
            KeyCode::PageDown => app.pane_scroll_page(1),
            KeyCode::PageUp => app.pane_scroll_page(-1),
            KeyCode::Home | KeyCode::Char('g') => app.pane_scroll_home(),
            KeyCode::End | KeyCode::Char('G') => app.pane_scroll_end(),
            _ => {}
        },
        Mode::Filter | Mode::LogFilter | Mode::Palette | Mode::Relocate => match code {
            KeyCode::Esc => {
                if app.mode == Mode::LogFilter {
                    app.cancel_log_filter_prompt();
                } else {
                    app.mode = Mode::List;
                    app.input.clear();
                    app.relocate_torrent_id = None;
                    app.clear_path_completion();
                    app.status.clear();
                }
            }
            KeyCode::Enter => {
                if app.mode == Mode::Filter {
                    app.apply_filter()?;
                    app.mode = Mode::List;
                    app.input.clear();
                } else if app.mode == Mode::LogFilter {
                    app.apply_log_filter();
                } else if app.mode == Mode::Relocate {
                    app.confirm_relocate()?;
                } else {
                    match app.run_palette()? {
                        PaletteAction::Quit => return Ok(true),
                        PaletteAction::None => {}
                    }
                    app.mode = Mode::List;
                    app.input.clear();
                }
            }
            KeyCode::Tab if app.mode == Mode::Relocate => {
                app.tab_complete_path();
            }
            KeyCode::Backspace => {
                app.input.pop();
                if app.mode == Mode::Relocate {
                    app.clear_path_completion();
                }
            }
            KeyCode::Char(c) => {
                // Ignore bare control chars when typing paths/commands.
                if !ctrl {
                    app.input.push(c);
                    if app.mode == Mode::Relocate {
                        app.clear_path_completion();
                    }
                }
            }
            _ => {}
        },
        Mode::Help => match code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?') | KeyCode::Char('h') => {
                app.mode = Mode::List;
                app.status.clear();
            }
            KeyCode::Char('j') | KeyCode::Down => app.pane_scroll(1),
            KeyCode::Char('k') | KeyCode::Up => app.pane_scroll(-1),
            KeyCode::PageDown => app.pane_scroll_page(1),
            KeyCode::PageUp => app.pane_scroll_page(-1),
            KeyCode::Home | KeyCode::Char('g') => app.pane_scroll_home(),
            KeyCode::End | KeyCode::Char('G') => app.pane_scroll_end(),
            _ => {}
        },
    }
    Ok(false)
}

/// Mouse wheel / left-click. Returns true if the UI should redraw.
fn handle_mouse(app: &mut App, m: MouseEvent) -> bool {
    match m.kind {
        MouseEventKind::Down(MouseButton::Left) => handle_mouse_click(app, m.column, m.row),
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => handle_mouse_scroll(app, m.kind),
        _ => false,
    }
}

/// Left-click hit-test. List: select the clicked data row (header clears selection).
fn handle_mouse_click(app: &mut App, col: u16, row: u16) -> bool {
    match app.mode {
        Mode::List => list_click_select(app, col, row),
        _ => false,
    }
}

/// Map terminal (col, row) onto the torrent list: border + column header + data rows.
fn list_click_select(app: &mut App, col: u16, row: u16) -> bool {
    let area = app.body_area;
    if area.width == 0 || area.height == 0 {
        return false;
    }
    if row < area.y
        || row >= area.y.saturating_add(area.height)
        || col < area.x
        || col >= area.x.saturating_add(area.width)
    {
        return false;
    }
    // Layout inside body: top border, table header, data rows…, bottom border.
    // Matches `draw_list` view_h = height.saturating_sub(3).
    let rel = row.saturating_sub(area.y);
    if rel == 0 {
        // Top border — ignore.
        return false;
    }
    if rel == 1 {
        // Column header — clear selection (like Space), keep scroll position.
        if app.selected.is_some() {
            app.deselect_keep_scroll();
            return true;
        }
        return false;
    }
    let data_row = (rel - 2) as usize;
    let view_h = area.height.saturating_sub(3).max(1) as usize;
    if data_row >= view_h {
        // Bottom border / padding past last visible row.
        return false;
    }
    let idx = app.list_table_state.offset().saturating_add(data_row);
    if idx >= app.rows.len() {
        return false;
    }
    app.select_index(idx);
    true
}

/// Mouse wheel → same direction as k/j (or list selection) for the active mode.
fn handle_mouse_scroll(app: &mut App, kind: MouseEventKind) -> bool {
    let up = matches!(kind, MouseEventKind::ScrollUp);
    let down = matches!(kind, MouseEventKind::ScrollDown);
    if !up && !down {
        return false;
    }
    match app.mode {
        // Log: up = older, down = newer (inverted vs document scroll).
        Mode::Log | Mode::LogFilter => {
            if up {
                app.log_scroll(1);
            } else {
                app.log_scroll(-1);
            }
            true
        }
        Mode::Detail => {
            app.detail_scroll(if up { -1 } else { 1 });
            true
        }
        Mode::Status | Mode::Help | Mode::LogHelp => {
            app.pane_scroll(if up { -1 } else { 1 });
            true
        }
        Mode::List => {
            if up {
                app.select_prev();
            } else {
                app.select_next();
            }
            true
        }
        Mode::Peers => {
            app.peer_select_delta(if up { -1 } else { 1 });
            true
        }
        Mode::Files => {
            app.file_select_delta(if up { -1 } else { 1 });
            true
        }
        Mode::Filter | Mode::Palette | Mode::Relocate => false,
    }
}

/// Non-interactive table print (also used when stdout is not a TTY).
pub fn run_plain_list(db: &Path) -> Result<()> {
    println!(
        "seedchamp {} (engine {})",
        env!("CARGO_PKG_VERSION"),
        ENGINE_VERSION
    );
    if !db.exists() {
        println!(
            "catalog: {} (missing — run `seedchamp torrent add` or `import` first)",
            db.display()
        );
        return Ok(());
    }
    let cat = seedchamp_engine::Catalog::open(db)?;
    let rows = cat.list_torrents()?;
    println!("catalog: {}  torrents: {}", db.display(), rows.len());
    println!();
    println!(
        "{:<6} {:<40} {:>12} {:>8} {:<6} {:<10} {}",
        "ID", "NAME", "SIZE", "HAVE", "RUN", "STATE", "INFOHASH"
    );
    println!("{}", "-".repeat(110));
    for r in &rows {
        let have = if r.complete {
            "done".into()
        } else {
            format!("{}/{}", r.have_count, r.piece_count)
        };
        let run = if r.want_start {
            "on"
        } else if r.error_msg.as_ref().is_some_and(|e| !e.is_empty()) {
            "err"
        } else {
            "off"
        };
        let name = ui::truncate(&r.name, 40);
        println!(
            "{:<6} {:<40} {:>12} {:>8} {:<6} {:<10} {}",
            r.id,
            name,
            ui::human_bytes(r.total_size),
            have,
            run,
            r.state,
            &r.infohash_hex[..8.min(r.infohash_hex.len())]
        );
    }
    let lim = cat.session_limits()?;
    println!();
    println!(
        "limits: up={} down={} peers={}",
        format_limit(lim.max_upload_bps),
        format_limit(lim.max_download_bps),
        lim.max_peers
    );
    Ok(())
}

fn format_limit(bps: u64) -> String {
    if bps == 0 {
        "∞".into()
    } else {
        format!("{}/s", ui::human_bytes(bps))
    }
}
