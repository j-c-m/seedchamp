//! Semantic TUI theme (colors only).
//!
//! Main `config.toml` holds a pointer (`tui.theme = "default" | "soft" | path`).
//! Theme files live under `$XDG_CONFIG_HOME/seedchamp/themes/` and contain only
//! color roles — no swarm/path settings.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use ratatui::style::{Color, Modifier, Style};
use serde::Deserialize;

use seedchamp_engine::{Error, Result};

/// Runtime theme: semantic colors + style helpers for draw code.
#[derive(Debug, Clone)]
pub struct Theme {
    pub name: String,
    /// Optional full-frame background (`None` = leave terminal default).
    pub canvas_bg: Option<Color>,
    pub header_fg: Color,
    pub header_bg: Color,
    pub footer_fg: Color,
    pub footer_bg: Color,
    pub status_fg: Color,
    pub border: Color,
    pub border_focus: Color,
    pub selected_fg: Color,
    pub selected_bg: Color,
    /// Muted text that stays readable on the selected row (e.g. RUN `off`).
    pub selected_muted: Color,
    pub text: Color,
    pub muted: Color,
    pub accent: Color,
    pub section: Color,
    pub label: Color,
    pub ok: Color,
    pub warn: Color,
    pub error: Color,
    pub run_on: Color,
    pub run_off: Color,
    pub run_check: Color,
    pub run_err: Color,
    pub recheck_row: Color,
    pub progress_done: Color,
    pub progress_partial: Color,
    pub progress_empty: Color,
    pub file_on: Color,
    pub file_off: Color,
    pub file_mix: Color,
    pub log_error: Color,
    pub log_warn: Color,
    pub log_info: Color,
    pub log_debug: Color,
    pub log_trace: Color,
    pub log_time: Color,
    pub log_target: Color,
    pub popup_border: Color,
    pub popup_bg: Color,
}

#[derive(Debug, Deserialize)]
struct ThemeFile {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    colors: HashMap<String, String>,
}

impl Theme {
    /// Current TUI look: named ANSI only (pixel-parity with pre-theme UI).
    pub fn default_ansi() -> Self {
        Self {
            name: "default".into(),
            canvas_bg: None,
            header_fg: Color::Black,
            header_bg: Color::Cyan,
            footer_fg: Color::White,
            footer_bg: Color::DarkGray,
            status_fg: Color::Yellow,
            // Reset matches pre-theme panels (no explicit border color).
            border: Color::Reset,
            border_focus: Color::Cyan,
            selected_fg: Color::White,
            selected_bg: Color::Blue,
            selected_muted: Color::White,
            text: Color::Reset,
            muted: Color::DarkGray,
            accent: Color::Cyan,
            section: Color::Yellow,
            label: Color::DarkGray,
            ok: Color::Green,
            warn: Color::Yellow,
            error: Color::Red,
            run_on: Color::Cyan,
            run_off: Color::DarkGray,
            run_check: Color::Yellow,
            run_err: Color::Red,
            recheck_row: Color::Yellow,
            progress_done: Color::Green,
            progress_partial: Color::Cyan,
            progress_empty: Color::DarkGray,
            file_on: Color::Green,
            file_off: Color::DarkGray,
            file_mix: Color::Yellow,
            log_error: Color::Red,
            log_warn: Color::Yellow,
            log_info: Color::Green,
            log_debug: Color::Cyan,
            log_trace: Color::DarkGray,
            log_time: Color::DarkGray,
            log_target: Color::Blue,
            popup_border: Color::Cyan,
            popup_bg: Color::Reset,
        }
    }

    /// Soft truecolor palette: modernized 90s/early-2000s terminal (teal/amber).
    pub fn soft_truecolor() -> Self {
        Self {
            name: "soft".into(),
            canvas_bg: Some(rgb(0x0b, 0x10, 0x20)),
            header_fg: rgb(0xe8, 0xf4, 0xf8),
            header_bg: rgb(0x1a, 0x6b, 0x7a),
            footer_fg: rgb(0xc5, 0xd0, 0xd8),
            footer_bg: rgb(0x12, 0x18, 0x2a),
            status_fg: rgb(0xe6, 0xc0, 0x7b),
            border: rgb(0x3d, 0x4f, 0x66),
            border_focus: rgb(0x5e, 0xc8, 0xd8),
            selected_fg: rgb(0xf0, 0xf6, 0xfa),
            selected_bg: rgb(0x1e, 0x3a, 0x5f),
            selected_muted: rgb(0xa8, 0xb8, 0xc8),
            text: Color::Reset,
            muted: rgb(0x6b, 0x7c, 0x8f),
            accent: rgb(0x5e, 0xc8, 0xd8),
            section: rgb(0xe6, 0xc0, 0x7b),
            label: rgb(0x7a, 0x8a, 0x9a),
            ok: rgb(0x6b, 0xcb, 0x8f),
            warn: rgb(0xe6, 0xc0, 0x7b),
            error: rgb(0xe0, 0x6c, 0x75),
            run_on: rgb(0x5e, 0xc8, 0xd8),
            run_off: rgb(0x6b, 0x7c, 0x8f),
            run_check: rgb(0xe6, 0xc0, 0x7b),
            run_err: rgb(0xe0, 0x6c, 0x75),
            recheck_row: rgb(0xe6, 0xc0, 0x7b),
            progress_done: rgb(0x6b, 0xcb, 0x8f),
            progress_partial: rgb(0x5e, 0xc8, 0xd8),
            progress_empty: rgb(0x6b, 0x7c, 0x8f),
            file_on: rgb(0x6b, 0xcb, 0x8f),
            file_off: rgb(0x6b, 0x7c, 0x8f),
            file_mix: rgb(0xe6, 0xc0, 0x7b),
            log_error: rgb(0xe0, 0x6c, 0x75),
            log_warn: rgb(0xe6, 0xc0, 0x7b),
            log_info: rgb(0x6b, 0xcb, 0x8f),
            log_debug: rgb(0x5e, 0xc8, 0xd8),
            log_trace: rgb(0x6b, 0x7c, 0x8f),
            log_time: rgb(0x6b, 0x7c, 0x8f),
            log_target: rgb(0x7a, 0xa2, 0xf7),
            popup_border: rgb(0x5e, 0xc8, 0xd8),
            popup_bg: rgb(0x12, 0x18, 0x2a),
        }
    }

    /// Load theme by builtin name or path (see module docs).
    pub fn load(spec: &str, config_dir: &Path) -> Result<Self> {
        let spec = spec.trim();
        if spec.is_empty() || spec.eq_ignore_ascii_case("default") {
            return Ok(Self::default_ansi());
        }
        if spec.eq_ignore_ascii_case("soft") {
            return Ok(Self::soft_truecolor());
        }

        let path = resolve_theme_path(spec, config_dir)?;
        Self::load_file(&path)
    }

    /// Parse a theme TOML file, merging onto [`Self::default_ansi`].
    pub fn load_file(path: &Path) -> Result<Self> {
        let text =
            fs::read_to_string(path).map_err(|e| Error::Path(path.to_path_buf(), e.to_string()))?;
        let file: ThemeFile = toml::from_str(&text)
            .map_err(|e| Error::Msg(format!("theme {}: {e}", path.display())))?;
        let mut theme = Self::default_ansi();
        if let Some(n) = file.name.filter(|s| !s.trim().is_empty()) {
            theme.name = n;
        } else if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            theme.name = stem.to_string();
        }
        theme.apply_overrides(&file.colors)?;
        Ok(theme)
    }

    /// Builtin theme TOML bodies for `config init` / documentation.
    pub fn builtin_toml(name: &str) -> Option<&'static str> {
        match name {
            "default" => Some(DEFAULT_THEME_TOML),
            "soft" => Some(SOFT_THEME_TOML),
            _ => None,
        }
    }

    /// Write stock theme files under `config_dir/themes/` (skips existing unless `force`).
    pub fn write_stock_themes(config_dir: &Path, force: bool) -> Result<Vec<PathBuf>> {
        let dir = config_dir.join("themes");
        fs::create_dir_all(&dir).map_err(|e| Error::Path(dir.clone(), e.to_string()))?;
        let mut written = Vec::new();
        for name in ["default", "soft"] {
            let path = dir.join(format!("{name}.toml"));
            if path.is_file() && !force {
                continue;
            }
            let body = Self::builtin_toml(name).expect("stock theme");
            fs::write(&path, body).map_err(|e| Error::Path(path.clone(), e.to_string()))?;
            written.push(path);
        }
        Ok(written)
    }

    fn apply_overrides(&mut self, colors: &HashMap<String, String>) -> Result<()> {
        for (key, raw) in colors {
            let key = key.trim().to_ascii_lowercase().replace('-', "_");
            let c = parse_theme_color(raw)
                .map_err(|e| Error::Msg(format!("theme color '{key}': {e} (value {raw:?})")))?;
            match key.as_str() {
                "canvas_bg" => {
                    self.canvas_bg = match c {
                        Color::Reset => None,
                        other => Some(other),
                    };
                }
                "header_fg" => self.header_fg = c,
                "header_bg" => self.header_bg = c,
                "footer_fg" => self.footer_fg = c,
                "footer_bg" => self.footer_bg = c,
                "status_fg" => self.status_fg = c,
                "border" => self.border = c,
                "border_focus" => self.border_focus = c,
                "selected_fg" => self.selected_fg = c,
                "selected_bg" => self.selected_bg = c,
                "selected_muted" => self.selected_muted = c,
                "text" => self.text = c,
                "muted" => self.muted = c,
                "accent" => self.accent = c,
                "section" => self.section = c,
                "label" => self.label = c,
                "ok" => self.ok = c,
                "warn" => self.warn = c,
                "error" => self.error = c,
                "run_on" => self.run_on = c,
                "run_off" => self.run_off = c,
                "run_check" => self.run_check = c,
                "run_err" => self.run_err = c,
                "recheck_row" => self.recheck_row = c,
                "progress_done" => self.progress_done = c,
                "progress_partial" => self.progress_partial = c,
                "progress_empty" => self.progress_empty = c,
                "file_on" => self.file_on = c,
                "file_off" => self.file_off = c,
                "file_mix" => self.file_mix = c,
                "log_error" => self.log_error = c,
                "log_warn" => self.log_warn = c,
                "log_info" => self.log_info = c,
                "log_debug" => self.log_debug = c,
                "log_trace" => self.log_trace = c,
                "log_time" => self.log_time = c,
                "log_target" => self.log_target = c,
                "popup_border" => self.popup_border = c,
                "popup_bg" => self.popup_bg = c,
                other => {
                    return Err(Error::Msg(format!("unknown theme color role '{other}'")));
                }
            }
        }
        Ok(())
    }

    // ── Style helpers ────────────────────────────────────────────────────

    pub fn selected_row(&self) -> Style {
        Style::default()
            .bg(self.selected_bg)
            .fg(self.selected_fg)
            .add_modifier(Modifier::BOLD)
    }

    pub fn header_bar(&self) -> Style {
        Style::default()
            .fg(self.header_fg)
            .bg(self.header_bg)
            .add_modifier(Modifier::BOLD)
    }

    pub fn footer_bar(&self) -> Style {
        Style::default().fg(self.footer_fg).bg(self.footer_bg)
    }

    pub fn status_line(&self) -> Style {
        Style::default().fg(self.status_fg)
    }

    pub fn panel_border(&self) -> Style {
        Style::default().fg(self.border)
    }

    pub fn focus_border(&self) -> Style {
        Style::default().fg(self.border_focus)
    }

    pub fn section_style(&self) -> Style {
        Style::default()
            .fg(self.section)
            .add_modifier(Modifier::BOLD)
    }

    pub fn label_style(&self) -> Style {
        Style::default().fg(self.label)
    }

    pub fn muted_style(&self) -> Style {
        Style::default().fg(self.muted)
    }

    pub fn accent_style(&self) -> Style {
        Style::default()
            .fg(self.accent)
            .add_modifier(Modifier::BOLD)
    }

    pub fn log_level(&self, level: char) -> (Color, &'static str) {
        match level {
            'E' => (self.log_error, "ERR"),
            'W' => (self.log_warn, "WRN"),
            'I' => (self.log_info, "INF"),
            'D' => (self.log_debug, "DBG"),
            'T' => (self.log_trace, "TRC"),
            _ => (self.text, "???"),
        }
    }
}

fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::Rgb(r, g, b)
}

fn parse_theme_color(s: &str) -> std::result::Result<Color, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty color".into());
    }
    Color::from_str(s).map_err(|e| e.to_string())
}

fn resolve_theme_path(spec: &str, config_dir: &Path) -> Result<PathBuf> {
    let p = PathBuf::from(spec);
    if p.is_absolute() {
        if p.is_file() {
            return Ok(p);
        }
        return Err(Error::Msg(format!("theme file not found: {}", p.display())));
    }

    // Bare name → themes/<name>.toml
    let bare = config_dir.join("themes").join(format!("{spec}.toml"));
    if bare.is_file() {
        return Ok(bare);
    }

    // Relative path from config dir
    let from_cfg = config_dir.join(spec);
    if from_cfg.is_file() {
        return Ok(from_cfg);
    }

    // CWD
    if p.is_file() {
        return Ok(p);
    }

    Err(Error::Msg(format!(
        "theme not found: {spec:?} (tried builtin, {}, {}, {})",
        bare.display(),
        from_cfg.display(),
        p.display()
    )))
}

// Use r## so values like "#0b1020" do not terminate the raw string.
const DEFAULT_THEME_TOML: &str = r##"# seedchamp TUI theme — default (ANSI, matches built-in look)
name = "default"

[colors]
# canvas_bg omitted = terminal default
header_fg = "black"
header_bg = "cyan"
footer_fg = "white"
footer_bg = "darkgray"
status_fg = "yellow"
border = "reset"
border_focus = "cyan"
selected_fg = "white"
selected_bg = "blue"
selected_muted = "white"
text = "reset"
muted = "darkgray"
accent = "cyan"
section = "yellow"
label = "darkgray"
ok = "green"
warn = "yellow"
error = "red"
run_on = "cyan"
run_off = "darkgray"
run_check = "yellow"
run_err = "red"
recheck_row = "yellow"
progress_done = "green"
progress_partial = "cyan"
progress_empty = "darkgray"
file_on = "green"
file_off = "darkgray"
file_mix = "yellow"
log_error = "red"
log_warn = "yellow"
log_info = "green"
log_debug = "cyan"
log_trace = "darkgray"
log_time = "darkgray"
log_target = "blue"
popup_border = "cyan"
popup_bg = "reset"
"##;

const SOFT_THEME_TOML: &str = r##"# seedchamp TUI theme — soft truecolor (modernized 90s terminal)
name = "soft"

[colors]
canvas_bg = "#0b1020"
header_fg = "#e8f4f8"
header_bg = "#1a6b7a"
footer_fg = "#c5d0d8"
footer_bg = "#12182a"
status_fg = "#e6c07b"
border = "#3d4f66"
border_focus = "#5ec8d8"
selected_fg = "#f0f6fa"
selected_bg = "#1e3a5f"
selected_muted = "#a8b8c8"
text = "reset"
muted = "#6b7c8f"
accent = "#5ec8d8"
section = "#e6c07b"
label = "#7a8a9a"
ok = "#6bcb8f"
warn = "#e6c07b"
error = "#e06c75"
run_on = "#5ec8d8"
run_off = "#6b7c8f"
run_check = "#e6c07b"
run_err = "#e06c75"
recheck_row = "#e6c07b"
progress_done = "#6bcb8f"
progress_partial = "#5ec8d8"
progress_empty = "#6b7c8f"
file_on = "#6bcb8f"
file_off = "#6b7c8f"
file_mix = "#e6c07b"
log_error = "#e06c75"
log_warn = "#e6c07b"
log_info = "#6bcb8f"
log_debug = "#5ec8d8"
log_trace = "#6b7c8f"
log_time = "#6b7c8f"
log_target = "#7aa2f7"
popup_border = "#5ec8d8"
popup_bg = "#12182a"
"##;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_ansi_selected_is_blue_white() {
        let t = Theme::default_ansi();
        assert_eq!(t.selected_bg, Color::Blue);
        assert_eq!(t.selected_fg, Color::White);
        assert_eq!(t.header_bg, Color::Cyan);
        assert!(t.canvas_bg.is_none());
    }

    #[test]
    fn soft_has_canvas_and_rgb() {
        let t = Theme::soft_truecolor();
        assert!(matches!(t.canvas_bg, Some(Color::Rgb(0x0b, 0x10, 0x20))));
        assert!(matches!(t.accent, Color::Rgb(0x5e, 0xc8, 0xd8)));
    }

    #[test]
    fn load_builtins() {
        let dir = Path::new("/nonexistent");
        assert_eq!(Theme::load("default", dir).unwrap().name, "default");
        assert_eq!(Theme::load("soft", dir).unwrap().name, "soft");
        assert_eq!(Theme::load("", dir).unwrap().name, "default");
    }

    #[test]
    fn partial_file_merges() {
        let dir = tempfile_dir();
        let path = dir.join("partial.toml");
        fs::write(
            &path,
            r##"
name = "partial"
[colors]
header_bg = "#ff0000"
status_fg = "magenta"
"##,
        )
        .unwrap();
        let t = Theme::load_file(&path).unwrap();
        assert_eq!(t.name, "partial");
        assert_eq!(t.header_bg, Color::Rgb(255, 0, 0));
        assert_eq!(t.status_fg, Color::Magenta);
        // untouched defaults
        assert_eq!(t.selected_bg, Color::Blue);
        assert_eq!(t.ok, Color::Green);
    }

    #[test]
    fn soft_toml_matches_builtin() {
        let dir = tempfile_dir();
        Theme::write_stock_themes(&dir, true).unwrap();
        let from_file = Theme::load_file(&dir.join("themes/soft.toml")).unwrap();
        let builtin = Theme::soft_truecolor();
        assert_eq!(from_file.header_bg, builtin.header_bg);
        assert_eq!(from_file.canvas_bg, builtin.canvas_bg);
        assert_eq!(from_file.accent, builtin.accent);
        assert_eq!(from_file.log_target, builtin.log_target);
    }

    #[test]
    fn unknown_role_errors() {
        let dir = tempfile_dir();
        let path = dir.join("bad.toml");
        fs::write(
            &path,
            r#"
[colors]
not_a_role = "red"
"#,
        )
        .unwrap();
        let err = Theme::load_file(&path).unwrap_err().to_string();
        assert!(err.contains("unknown theme color role"), "{err}");
    }

    #[test]
    fn missing_theme_errors() {
        let err = Theme::load("nope", Path::new("/tmp"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("theme not found"), "{err}");
    }

    fn tempfile_dir() -> PathBuf {
        let mut d = std::env::temp_dir();
        d.push(format!(
            "seedchamp-theme-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }
}
