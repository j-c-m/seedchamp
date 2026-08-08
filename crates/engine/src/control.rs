//! Control plane: TUI → commands → engine; engine → events → TUI.
//!
//! **Liveness (UI must never hang):**
//! - TUI only `try_send`s commands and `try_recv`s events — never waits.
//! - Snapshot reads shared `SessionRuntime` with `try_read` locks.
//! - Start/stop/file-priority run on a **serial mutation worker** (ordered start→stop→start).
//! - Full catalog **list** (and other RO SQL) run on a **catalog reader** thread so
//!   scans never block mutations.
//! - Recheck is **detached**: mutate only claims + spawns a recheck thread; piece work
//!   uses the hash pool. Start/stop are not blocked by a long recheck.
//! - Replies arrive as [`ControlEvent`]s; TUI updates status when polled.
//!
//! Command/event buses use `std::sync::mpsc` so the control thread (not the
//! Compio peer/accept runtimes) can block/receive reliably.

mod handle;
mod mutation;
mod plane;
mod reader;
mod types;

#[cfg(test)]
mod tests;

pub use handle::{ControlHandle, ControlPlane, RuntimeInfo};
pub use plane::spawn_control_plane;
pub use types::{ControlEvent, EngineCommand};
