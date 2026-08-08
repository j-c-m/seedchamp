//! In-process activity log for the TUI (and optional CLI diagnostics).
//!
//! A [`tracing`] layer appends events into a fixed-capacity ring. The TUI polls
//! [`ActivityLog::snapshot`] without blocking the peer path for long.

use std::collections::VecDeque;
use std::fmt::{self, Write as _};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer};

/// One line shown on the TUI log screen.
#[derive(Debug, Clone)]
pub struct LogLine {
    pub seq: u64,
    /// Local wall-clock `HH:MM:SS` (best effort).
    pub time: String,
    pub level: char,
    /// Short target (`session`, `watch`, …).
    pub target: String,
    pub message: String,
}

/// Ring buffer of recent log lines (thread-safe).
pub struct ActivityLog {
    inner: Mutex<Inner>,
    /// Monotonic sequence; TUI uses this to detect new lines cheaply.
    seq: AtomicU64,
    /// Current capture directive (e.g. `info`, `seedchamp_engine=debug`).
    capture: Mutex<String>,
    /// Reloads the process [`EnvFilter`] (set once during [`init_activity_logging`]).
    reload: Mutex<Option<CaptureReload>>,
}

/// Type-erased handle so we do not expose layered subscriber types publicly.
struct CaptureReload {
    set: Box<dyn Fn(&str) -> std::result::Result<(), String> + Send + Sync>,
}

impl fmt::Debug for ActivityLog {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ActivityLog")
            .field("seq", &self.seq.load(Ordering::Relaxed))
            .field("len", &self.len())
            .field("capture", &*self.capture.lock())
            .finish()
    }
}

#[derive(Debug)]
struct Inner {
    lines: VecDeque<LogLine>,
    capacity: usize,
}

impl ActivityLog {
    pub fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner {
                lines: VecDeque::with_capacity(capacity.max(64)),
                capacity: capacity.max(64),
            }),
            seq: AtomicU64::new(0),
            capture: Mutex::new("info".into()),
            reload: Mutex::new(None),
        })
    }

    pub fn seq(&self) -> u64 {
        self.seq.load(Ordering::Relaxed)
    }

    pub fn len(&self) -> usize {
        self.inner.lock().lines.len()
    }

    /// Current capture filter directive (what tracing accepts into the ring).
    pub fn capture_filter(&self) -> String {
        self.capture.lock().clone()
    }

    /// Change capture level at runtime (e.g. `debug`, `warn`, `seedchamp_engine=trace`).
    ///
    /// Does **not** rewrite past ring lines — only affects new events.
    /// Display filter on the TUI log screen is independent.
    pub fn set_capture_filter(&self, directive: &str) -> std::result::Result<(), String> {
        let d = normalize_capture_directive(directive)?;
        let guard = self.reload.lock();
        let Some(reloader) = guard.as_ref() else {
            return Err("logging not initialized with a reloadable filter".into());
        };
        (reloader.set)(&d)?;
        drop(guard);
        *self.capture.lock() = d.clone();
        self.info("tui", format!("capture filter → {d}"));
        Ok(())
    }

    /// Cycle error → warn → info → debug → trace → error …
    pub fn cycle_capture_level(&self) -> std::result::Result<String, String> {
        let cur = self.capture.lock().clone();
        let next = next_simple_level(&cur);
        self.set_capture_filter(next)?;
        Ok(next.to_string())
    }

    fn attach_reload<F>(&self, f: F)
    where
        F: Fn(&str) -> std::result::Result<(), String> + Send + Sync + 'static,
    {
        *self.reload.lock() = Some(CaptureReload { set: Box::new(f) });
    }

    /// Push a synthetic line (not from tracing).
    pub fn push(&self, level: char, target: impl Into<String>, message: impl Into<String>) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        let line = LogLine {
            seq,
            time: local_hms(),
            level,
            target: short_target(&target.into()),
            message: message.into(),
        };
        let mut g = self.inner.lock();
        if g.lines.len() >= g.capacity {
            g.lines.pop_front();
        }
        g.lines.push_back(line);
    }

    pub fn info(&self, target: impl Into<String>, message: impl Into<String>) {
        self.push('I', target, message);
    }

    /// Clone current lines for the UI (newest last).
    pub fn snapshot(&self) -> Vec<LogLine> {
        self.inner.lock().lines.iter().cloned().collect()
    }
}

/// Accept plain levels or full EnvFilter directives.
fn normalize_capture_directive(directive: &str) -> std::result::Result<String, String> {
    let d = directive.trim();
    if d.is_empty() {
        return Err("empty log filter".into());
    }
    // Bare level names → apply globally (same as EnvFilter default).
    let d = match d.to_ascii_lowercase().as_str() {
        "error" | "err" | "e" => "error",
        "warn" | "warning" | "wrn" | "w" => "warn",
        "info" | "inf" | "i" => "info",
        "debug" | "dbg" | "d" => "debug",
        "trace" | "trc" | "t" => "trace",
        "off" => "off",
        _ => d, // keep full directives like `seedchamp_engine=debug,warn`
    };
    // Validate early.
    EnvFilter::try_new(d).map_err(|e| format!("bad filter {d:?}: {e}"))?;
    Ok(d.to_string())
}

fn next_simple_level(current: &str) -> &'static str {
    // If a complex directive, jump to debug as the "more verbose" step.
    let base = current
        .split(',')
        .next()
        .unwrap_or(current)
        .split('=')
        .next_back()
        .unwrap_or(current)
        .trim()
        .to_ascii_lowercase();
    match base.as_str() {
        "error" | "err" => "warn",
        "warn" | "warning" => "info",
        "info" => "debug",
        "debug" => "trace",
        "trace" => "error",
        "off" => "error",
        _ => "debug",
    }
}

fn local_hms() -> String {
    match time::OffsetDateTime::now_local() {
        Ok(t) => format!("{:02}:{:02}:{:02}", t.hour(), t.minute(), t.second()),
        Err(_) => {
            let t = time::OffsetDateTime::now_utc();
            format!("{:02}:{:02}:{:02}Z", t.hour(), t.minute(), t.second())
        }
    }
}

fn short_target(target: &str) -> String {
    // `seedchamp_engine::session` → `session`
    target
        .rsplit("::")
        .next()
        .unwrap_or(target)
        .chars()
        .take(16)
        .collect()
}

fn level_char(level: &Level) -> char {
    match *level {
        Level::ERROR => 'E',
        Level::WARN => 'W',
        Level::INFO => 'I',
        Level::DEBUG => 'D',
        Level::TRACE => 'T',
    }
}

/// Tracing layer that records events into an [`ActivityLog`].
pub struct ActivityLogLayer {
    log: Arc<ActivityLog>,
}

impl ActivityLogLayer {
    pub fn new(log: Arc<ActivityLog>) -> Self {
        Self { log }
    }
}

struct MessageVisitor {
    message: String,
    fields: String,
}

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.message, "{value:?}");
            // Debug quotes strings; strip a single pair of quotes when present.
            if self.message.starts_with('"')
                && self.message.ends_with('"')
                && self.message.len() >= 2
            {
                self.message = self.message[1..self.message.len() - 1].to_string();
            }
        } else {
            if !self.fields.is_empty() {
                self.fields.push(' ');
            }
            let _ = write!(self.fields, "{}={value:?}", field.name());
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message.push_str(value);
        } else {
            if !self.fields.is_empty() {
                self.fields.push(' ');
            }
            let _ = write!(self.fields, "{}={value}", field.name());
        }
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        if !self.fields.is_empty() {
            self.fields.push(' ');
        }
        let _ = write!(self.fields, "{}={value}", field.name());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        if !self.fields.is_empty() {
            self.fields.push(' ');
        }
        let _ = write!(self.fields, "{}={value}", field.name());
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        if !self.fields.is_empty() {
            self.fields.push(' ');
        }
        let _ = write!(self.fields, "{}={value}", field.name());
    }
}

impl<S> Layer<S> for ActivityLogLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        let mut vis = MessageVisitor {
            message: String::new(),
            fields: String::new(),
        };
        event.record(&mut vis);
        let mut msg = vis.message;
        if msg.is_empty() {
            msg = vis.fields;
        } else if !vis.fields.is_empty() {
            msg.push(' ');
            msg.push_str(&vis.fields);
        }
        if msg.is_empty() {
            msg = meta.name().to_string();
        }
        // Cap line length so a huge debug dump cannot blow the TUI.
        if msg.len() > 400 {
            msg.truncate(397);
            msg.push_str("...");
        }
        self.log
            .push(level_char(meta.level()), meta.target().to_string(), msg);
    }
}

/// Install the global tracing subscriber with an activity-log layer.
///
/// Safe to call once per process. Uses `try_init` so tests / double start do not
/// panic. Caller keeps the returned [`Arc`] for the TUI to snapshot.
///
/// `level` is an [`EnvFilter`] directive (`info`, `seedchamp_engine=debug`, …).
/// `RUST_LOG` overrides when set at init time; afterward use
/// [`ActivityLog::set_capture_filter`].
pub fn init_activity_logging(level: &str, capacity: usize) -> Arc<ActivityLog> {
    let log = ActivityLog::new(capacity);
    let initial = std::env::var("RUST_LOG")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| level.to_string());
    let initial = normalize_capture_directive(&initial).unwrap_or_else(|_| "info".into());
    *log.capture.lock() = initial.clone();

    let filter = EnvFilter::try_new(&initial).unwrap_or_else(|_| EnvFilter::new("info"));
    let (filter_layer, reload_handle) = tracing_subscriber::reload::Layer::new(filter);
    let layer = ActivityLogLayer::new(log.clone());

    // try_init — unit tests may already have a subscriber.
    let installed = tracing_subscriber::registry()
        .with(filter_layer)
        .with(layer)
        .try_init()
        .is_ok();

    if installed {
        let handle = reload_handle;
        log.attach_reload(move |directive: &str| {
            let f = EnvFilter::try_new(directive).map_err(|e| e.to_string())?;
            handle.reload(f).map_err(|e| e.to_string())
        });
        log.info(
            "tui",
            format!("activity log ready (cap={capacity}, capture={initial})"),
        );
    } else {
        // Still usable as a manual ring (push / TUI lines) without tracing capture.
        log.info(
            "tui",
            "activity log ready (subscriber already set — capture reload unavailable)",
        );
    }
    log
}
