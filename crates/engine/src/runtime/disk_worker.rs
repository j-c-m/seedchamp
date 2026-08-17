//! Dedicated disk **worker** for durable piece writes (off peer protocol path).
//!
//! Backends (selected at spawn):
//! - **Linux:** `io_uring` when probe succeeds (`SEEDCHAMP_DISK_BACKEND=uring|auto`)
//! - **FreeBSD / Darwin:** POSIX AIO when available (`aio|auto`)
//! - **Fallback:** dedicated thread + sync `pwrite` (always available)
//!
//! Env:
//! - `SEEDCHAMP_DISK_BACKEND=auto|thread|uring|aio` (default `auto`)
//! - `SEEDCHAMP_DISK_DEPTH` — max in-flight piece jobs (default 32, clamp 1–256)

mod spawn;
mod thread;

// io_uring / POSIX AIO require kernel FFI; contained here only.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
mod uring;

#[cfg(any(target_os = "freebsd", target_os = "macos"))]
#[allow(unsafe_code)]
mod aio;

#[cfg(test)]
mod tests;

pub use spawn::disk_depth_from_env;
pub(crate) use spawn::{complete_discard_job, complete_write_job, write_job_sync};
use spawn::{is_backend_dead, parse_backend_want, spawn_backend, BackendWant};

use std::sync::Arc;
use std::time::{Duration, Instant};

use flume::Sender as FlumeSender;
use parking_lot::Mutex;

use crate::disk::StorageLayout;
use crate::error::{Error, Result};

use super::hash_worker::HashOutcome;

pub use self::thread::ThreadBackend;

/// Default max concurrent piece write jobs (each holds a full piece buffer).
pub const DEFAULT_DISK_DEPTH: usize = 32;
const DISK_DEPTH_MAX: usize = 256;

/// Max times the disk thread may be respawned per session after a fatal stop.
pub const MAX_DISK_RESTARTS: u32 = 3;
/// Minimum time between restart attempts.
pub const DISK_RESTART_COOLDOWN: Duration = Duration::from_secs(5);

/// Operator-facing sticky message when the disk worker will not restart again.
pub const DISK_WORKER_DEAD_STATUS: &str = "disk worker dead — restart process";

pub struct DiskWriteJob {
    pub index: u32,
    pub plen: u32,
    pub data: Vec<u8>,
    /// Shared with hash job / hot torrent — avoid cloning thousands of file paths.
    pub layout: Arc<StorageLayout>,
    /// Reply to the leecher (same channel as hash outcomes).
    pub reply: FlumeSender<HashOutcome>,
}

/// Selected backend name (stable strings for logs / tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskBackendKind {
    Thread,
    Uring,
    Aio,
}

impl DiskBackendKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Thread => "thread",
            Self::Uring => "io_uring",
            Self::Aio => "aio",
        }
    }
}

/// Internal submit path shared by backends.
pub(crate) trait DiskWriteBackend: Send + Sync {
    fn submit(&self, job: DiskWriteJob) -> std::result::Result<(), (Error, DiskWriteJob)>;
}

type StatusHook = Arc<dyn Fn(String) + Send + Sync>;

struct DiskState {
    backend: Arc<dyn DiskWriteBackend>,
    kind: DiskBackendKind,
    restarts: u32,
    last_restart: Option<Instant>,
    /// After max restarts (or spawn failure with budget exhausted), never recover.
    dead_permanent: bool,
}

struct DiskInner {
    discard_writes: bool,
    depth: usize,
    want: BackendWant,
    state: Mutex<DiskState>,
    status_hook: Mutex<Option<StatusHook>>,
}

/// Cloneable handle to the session disk path (shared restart state).
#[derive(Clone)]
pub struct DiskWorker {
    inner: Arc<DiskInner>,
}

impl DiskWorker {
    /// Spawn with explicit backend name and depth (from config + env merge).
    ///
    /// `backend`: `auto` | `thread` | `uring` | `aio` (unknown → error).
    /// `depth`: clamped to 1..=256.
    pub fn spawn_with_options(discard_writes: bool, backend: &str, depth: usize) -> Result<Self> {
        let depth = depth.clamp(1, DISK_DEPTH_MAX);
        let want = parse_backend_want(backend)?;
        let (backend_impl, kind) = spawn_backend(want, depth, discard_writes)?;
        tracing::info!(
            backend = kind.as_str(),
            depth,
            discard_writes,
            "disk worker started"
        );
        Ok(Self {
            inner: Arc::new(DiskInner {
                discard_writes,
                depth,
                want,
                state: Mutex::new(DiskState {
                    backend: backend_impl,
                    kind,
                    restarts: 0,
                    last_restart: None,
                    dead_permanent: false,
                }),
                status_hook: Mutex::new(None),
            }),
        })
    }

    /// Optional session status line (TUI) — e.g. permanent disk death.
    pub fn set_status_hook(&self, hook: impl Fn(String) + Send + Sync + 'static) {
        *self.inner.status_hook.lock() = Some(Arc::new(hook));
    }

    /// Active backend (`thread` / `io_uring` / `aio`).
    pub fn backend_kind(&self) -> DiskBackendKind {
        self.inner.state.lock().kind
    }

    pub fn backend_name(&self) -> &'static str {
        self.backend_kind().as_str()
    }

    /// Configured piece-job depth (`SEEDCHAMP_DISK_DEPTH`).
    #[inline]
    pub fn depth(&self) -> usize {
        self.inner.depth
    }

    /// Bench / harness: skip durable piece writes after SHA-1.
    #[inline]
    pub fn discard_writes(&self) -> bool {
        self.inner.discard_writes
    }

    /// True after restart budget is exhausted.
    pub fn is_permanently_dead(&self) -> bool {
        self.inner.state.lock().dead_permanent
    }

    /// Restart attempts this session (successful respawn or failed spawn).
    pub fn restart_count(&self) -> u32 {
        self.inner.state.lock().restarts
    }

    /// On channel disconnect, may restart the backend then retry once.
    /// Returns the job so the caller can reclaim `data` on hard failure.
    pub fn submit_write(
        &self,
        job: DiskWriteJob,
    ) -> std::result::Result<(), (Error, DiskWriteJob)> {
        // Fast path without holding the job across a long lock if permanent.
        {
            let st = self.inner.state.lock();
            if st.dead_permanent {
                return Err((Error::DiskWorkerPermanent, job));
            }
        }

        let backend = self.inner.state.lock().backend.clone();
        match backend.submit(job) {
            Ok(()) => Ok(()),
            Err((e, job)) if is_backend_dead(&e) => {
                if self.try_restart(&e) {
                    let backend = self.inner.state.lock().backend.clone();
                    backend.submit(job)
                } else if self.inner.state.lock().dead_permanent {
                    Err((Error::DiskWorkerPermanent, job))
                } else {
                    Err((e, job))
                }
            }
            Err(other) => Err(other),
        }
    }

    /// Attempt to respawn the same configured backend. Returns true if a new
    /// backend is installed and the caller should retry submit.
    ///
    /// Budget/cooldown under `state`; `spawn_backend` and old-backend Drop/join
    /// run **outside** the mutex so concurrent submitters are not parked on join.
    fn try_restart(&self, cause: &Error) -> bool {
        let now = Instant::now();
        {
            let mut st = self.inner.state.lock();
            if st.dead_permanent {
                return false;
            }
            // Budget before cooldown: after max restarts, go permanent immediately.
            if st.restarts >= MAX_DISK_RESTARTS {
                st.dead_permanent = true;
                drop(st);
                self.emit_status(DISK_WORKER_DEAD_STATUS.to_string());
                tracing::error!(
                    error = %cause,
                    max = MAX_DISK_RESTARTS,
                    "disk worker dead permanently — restart process"
                );
                return false;
            }
            if let Some(last) = st.last_restart {
                if now.saturating_duration_since(last) < DISK_RESTART_COOLDOWN {
                    tracing::warn!(
                        error = %cause,
                        "disk worker dead; restart cooldown active"
                    );
                    return false;
                }
            }
            // Reserve a restart slot before unlocked spawn (serializes thundering herd).
            st.restarts += 1;
            st.last_restart = Some(now);
        }

        let spawn = spawn_backend(self.inner.want, self.inner.depth, self.inner.discard_writes);

        match spawn {
            Ok((backend, kind)) => {
                let (n, old) = {
                    let mut st = self.inner.state.lock();
                    if st.dead_permanent {
                        // Lost the race to permanent; drop new backend outside.
                        drop(st);
                        drop(backend);
                        return false;
                    }
                    let old = std::mem::replace(&mut st.backend, backend);
                    st.kind = kind;
                    (st.restarts, old)
                };
                drop(old); // join previous disk OS thread off the mutex
                tracing::error!(
                    error = %cause,
                    restart = n,
                    max = MAX_DISK_RESTARTS,
                    backend = kind.as_str(),
                    "disk worker restarted"
                );
                self.emit_status(format!("disk worker restarted ({n}/{MAX_DISK_RESTARTS})"));
                true
            }
            Err(spawn_err) => {
                let permanent = {
                    let mut st = self.inner.state.lock();
                    // restarts already incremented as the attempt
                    if st.restarts >= MAX_DISK_RESTARTS {
                        st.dead_permanent = true;
                        true
                    } else {
                        false
                    }
                };
                if permanent {
                    self.emit_status(DISK_WORKER_DEAD_STATUS.to_string());
                    tracing::error!(
                        error = %cause,
                        spawn = %spawn_err,
                        "disk worker respawn failed — permanently dead"
                    );
                } else {
                    tracing::error!(
                        error = %cause,
                        spawn = %spawn_err,
                        "disk worker respawn failed"
                    );
                }
                false
            }
        }
    }

    fn emit_status(&self, msg: String) {
        // Clone hook under lock; invoke after unlock so the callback may take
        // session locks without nesting under status_hook.
        let hook = self.inner.status_hook.lock().clone();
        if let Some(hook) = hook {
            hook(msg);
        }
    }

    /// Test helper: replace live backend (e.g. always-dead mock).
    #[cfg(test)]
    pub(crate) fn inject_backend_for_test(&self, backend: Arc<dyn DiskWriteBackend>) {
        self.inner.state.lock().backend = backend;
    }

    /// Test helper: pretreat restart counter / permanent flag.
    #[cfg(test)]
    pub(crate) fn set_restart_state_for_test(
        &self,
        restarts: u32,
        dead_permanent: bool,
        last_restart: Option<Instant>,
    ) {
        let mut st = self.inner.state.lock();
        st.restarts = restarts;
        st.dead_permanent = dead_permanent;
        st.last_restart = last_restart;
    }
}
