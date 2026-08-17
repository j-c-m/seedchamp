//! Non-blocking control handle for TUI/CLI.

use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use parking_lot::RwLock;

use crate::catalog::SessionLimits;
use crate::error::{Error, Result};
use crate::session::{SessionRuntime, SessionSnapshot};

use super::types::{ControlEvent, EngineCommand};

/// Cloneable handle for TUI / CLI — **never blocks** the UI thread.
#[derive(Clone)]
pub struct ControlHandle {
    pub(super) cmd_tx: mpsc::Sender<EngineCommand>,
    /// Events from engine; TUI drains with `try_recv` each frame.
    pub(super) event_rx: Arc<std::sync::Mutex<mpsc::Receiver<ControlEvent>>>,
    pub(super) session: Arc<RwLock<Option<SessionRuntime>>>,
}

impl ControlHandle {
    /// Fire-and-forget command. Never waits for completion.
    pub fn send(&self, cmd: EngineCommand) -> Result<()> {
        self.cmd_tx
            .send(cmd)
            .map_err(|_| Error::Msg("control plane stopped".into()))
    }

    /// Enqueue start; does not wait. Poll events for `Started` / `StartFailed`.
    pub fn request_start(&self, id: i64) -> Result<()> {
        self.send(EngineCommand::StartTorrent { id })
    }

    /// Enqueue stop; does not wait. Poll events for `Stopped` / `StopFailed`.
    pub fn request_stop(&self, id: i64) -> Result<()> {
        self.send(EngineCommand::StopTorrent { id })
    }

    /// Enqueue recheck; does not wait. Poll events for `Rechecked` / `RecheckFailed`.
    pub fn request_recheck(&self, id: i64) -> Result<()> {
        self.send(EngineCommand::Recheck { id })
    }

    /// Enqueue file priority change; does not wait.
    ///
    /// `priority` 0 = off, ≥1 = on.
    pub fn request_set_file_priority(
        &self,
        torrent_id: i64,
        file_idx: u32,
        priority: i32,
    ) -> Result<()> {
        self.send(EngineCommand::SetFilePriority {
            torrent_id,
            file_idx,
            priority,
        })
    }

    /// Enqueue live relocate (Ctrl-O); does not wait. Poll `Relocated` / `RelocateFailed`.
    pub fn request_relocate(&self, id: i64, new_root: PathBuf) -> Result<()> {
        self.send(EngineCommand::Relocate { id, new_root })
    }

    /// Enqueue soft-delete; does not wait. Poll `SoftDeleted` / `SoftDeleteFailed`.
    pub fn request_soft_delete(&self, id: i64) -> Result<()> {
        self.send(EngineCommand::SoftDelete { id })
    }

    /// Enqueue hard catalog remove; does not wait. Poll `Removed` / `RemoveFailed`.
    pub fn request_remove(&self, id: i64) -> Result<()> {
        self.send(EngineCommand::Remove { id })
    }

    /// Enqueue full session limits (catalog + live wire/peer caps). Poll `LimitsUpdated`.
    pub fn request_set_session_limits(&self, limits: SessionLimits) -> Result<()> {
        self.send(EngineCommand::SetSessionLimits { limits })
    }

    /// Enqueue full catalog list for TUI (catalog reader). Poll `CatalogList` / `CatalogListFailed`.
    pub fn request_list_catalog(&self, filter: String) -> Result<()> {
        self.send(EngineCommand::ListCatalog { filter })
    }

    /// Non-blocking: one event if ready.
    pub fn try_recv_event(&self) -> Option<ControlEvent> {
        self.event_rx.lock().ok().and_then(|rx| rx.try_recv().ok())
    }

    /// Drain all pending events (non-blocking).
    pub fn drain_events(&self) -> Vec<ControlEvent> {
        let mut out = Vec::new();
        while let Some(e) = self.try_recv_event() {
            out.push(e);
        }
        out
    }

    /// Shared session snapshot for TUI (non-blocking locks only).
    pub fn snapshot(&self) -> Result<SessionSnapshot> {
        let guard = self.session.read();
        match guard.as_ref() {
            Some(s) => Ok(s.snapshot_nonblocking()),
            None => Ok(SessionSnapshot::default()),
        }
    }

    pub fn runtime_info(&self) -> Result<RuntimeInfo> {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(s) = self.session.read().clone() {
                let snap = s.snapshot_nonblocking();
                return Ok(RuntimeInfo {
                    listen: snap.listen,
                    peer_workers: s.peer_workers(),
                    status: snap.status_line,
                });
            }
            if Instant::now() >= deadline {
                return Err(Error::Msg("control plane not ready".into()));
            }
            let _ = self.try_recv_event();
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Non-blocking shutdown: quit-time stopped announces + teardown.
    ///
    /// Poll [`Self::session_alive`] / TUI snapshot `status_line` until the session
    /// slot is cleared. Blocking wait: [`Self::shutdown`].
    pub fn request_shutdown(&self) {
        if self.send(EngineCommand::Shutdown).is_err() {
            *self.session.write() = None;
        }
    }

    /// True while the `SessionRuntime` is still in the control slot
    /// (including during quit-time `event=stopped` announces).
    pub fn session_alive(&self) -> bool {
        self.session.read().is_some()
    }

    /// Block until control finishes quit-time stopped announces and tears down.
    ///
    /// No wall-clock abort — force-clearing the session mid-announce would drop
    /// in-flight stopped events. Non-blocking: [`Self::request_shutdown`].
    pub fn shutdown(&self) {
        self.request_shutdown();
        while self.session_alive() {
            thread::sleep(Duration::from_millis(50));
        }
    }
}

#[derive(Debug, Clone)]
pub struct RuntimeInfo {
    pub listen: String,
    pub peer_workers: usize,
    pub status: String,
}

pub struct ControlPlane {
    pub(super) join: Option<thread::JoinHandle<()>>,
}

impl Drop for ControlPlane {
    fn drop(&mut self) {
        // Join so process exit does not kill mid quit-time stopped announces.
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}
