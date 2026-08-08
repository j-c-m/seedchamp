//! Backend selection and shared write completion helpers.

use std::sync::Arc;

use crate::error::{Error, Result};

use super::super::hash_worker::HashOutcome;
use super::{
    DiskBackendKind, DiskWriteBackend, DiskWriteJob, ThreadBackend, DEFAULT_DISK_DEPTH,
    DISK_DEPTH_MAX,
};

/// Channel disconnect — restart may apply. Permanent is handled on the fast path.
pub(super) fn is_backend_dead(e: &Error) -> bool {
    matches!(e, Error::DiskWorkerStopped)
}

/// Parse `SEEDCHAMP_DISK_DEPTH` (default 32, clamp 1..=256).
pub fn disk_depth_from_env() -> usize {
    match std::env::var("SEEDCHAMP_DISK_DEPTH") {
        Ok(s) => s
            .trim()
            .parse::<usize>()
            .unwrap_or(DEFAULT_DISK_DEPTH)
            .clamp(1, DISK_DEPTH_MAX),
        Err(_) => DEFAULT_DISK_DEPTH,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BackendWant {
    Auto,
    Thread,
    Uring,
    Aio,
}

pub(super) fn backend_want_string_from_env() -> String {
    std::env::var("SEEDCHAMP_DISK_BACKEND").unwrap_or_else(|_| "auto".into())
}

pub(super) fn parse_backend_want(s: &str) -> Result<BackendWant> {
    match s.trim().to_ascii_lowercase().as_str() {
        "" | "auto" => Ok(BackendWant::Auto),
        "thread" => Ok(BackendWant::Thread),
        "uring" => Ok(BackendWant::Uring),
        "aio" => Ok(BackendWant::Aio),
        other => Err(Error::Msg(format!(
            "unknown disk.backend {other:?} (auto|thread|uring|aio)"
        ))),
    }
}

pub(super) fn spawn_backend(
    want: BackendWant,
    depth: usize,
    discard_writes: bool,
) -> Result<(Arc<dyn DiskWriteBackend>, DiskBackendKind)> {
    match want {
        BackendWant::Thread => {
            let b = ThreadBackend::spawn(depth, discard_writes)?;
            Ok((Arc::new(b), DiskBackendKind::Thread))
        }
        BackendWant::Uring => {
            #[cfg(target_os = "linux")]
            {
                match super::uring::UringBackend::spawn(depth, discard_writes) {
                    Ok(b) => Ok((Arc::new(b), DiskBackendKind::Uring)),
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "io_uring backend failed; falling back to thread"
                        );
                        let b = ThreadBackend::spawn(depth, discard_writes)?;
                        Ok((Arc::new(b), DiskBackendKind::Thread))
                    }
                }
            }
            #[cfg(not(target_os = "linux"))]
            {
                tracing::warn!(
                    "SEEDCHAMP_DISK_BACKEND=uring not supported on this OS; using thread"
                );
                let b = ThreadBackend::spawn(depth, discard_writes)?;
                Ok((Arc::new(b), DiskBackendKind::Thread))
            }
        }
        BackendWant::Aio => {
            #[cfg(any(target_os = "freebsd", target_os = "macos"))]
            {
                match super::aio::AioBackend::spawn(depth, discard_writes) {
                    Ok(b) => Ok((Arc::new(b), DiskBackendKind::Aio)),
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "aio backend failed; falling back to thread"
                        );
                        let b = ThreadBackend::spawn(depth, discard_writes)?;
                        Ok((Arc::new(b), DiskBackendKind::Thread))
                    }
                }
            }
            #[cfg(not(any(target_os = "freebsd", target_os = "macos")))]
            {
                tracing::warn!("SEEDCHAMP_DISK_BACKEND=aio not supported on this OS; using thread");
                let b = ThreadBackend::spawn(depth, discard_writes)?;
                Ok((Arc::new(b), DiskBackendKind::Thread))
            }
        }
        BackendWant::Auto => {
            #[cfg(target_os = "linux")]
            {
                match super::uring::UringBackend::spawn(depth, discard_writes) {
                    Ok(b) => return Ok((Arc::new(b), DiskBackendKind::Uring)),
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "disk backend auto: io_uring probe failed; falling back to thread"
                        );
                    }
                }
            }
            #[cfg(any(target_os = "freebsd", target_os = "macos"))]
            {
                match super::aio::AioBackend::spawn(depth, discard_writes) {
                    Ok(b) => return Ok((Arc::new(b), DiskBackendKind::Aio)),
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "disk backend auto: aio probe failed; falling back to thread"
                        );
                    }
                }
            }
            let b = ThreadBackend::spawn(depth, discard_writes)?;
            Ok((Arc::new(b), DiskBackendKind::Thread))
        }
    }
}

/// Shared: build Ok/Fail and send on `job.reply`.
pub(crate) fn complete_write_job(job: DiskWriteJob, write_result: Result<()>) {
    let DiskWriteJob {
        index,
        plen,
        data,
        reply,
        ..
    } = job;
    let outcome = match write_result {
        Ok(()) => HashOutcome::Ok { index, plen, data },
        Err(e) => {
            tracing::warn!(piece = index, error = %e, "disk write failed");
            HashOutcome::WriteFail { index, plen, data }
        }
    };
    let _ = reply.send(outcome);
}

/// Discard path: no I/O, always Ok.
pub(crate) fn complete_discard_job(job: DiskWriteJob) {
    let DiskWriteJob {
        index,
        plen,
        data,
        reply,
        ..
    } = job;
    let _ = reply.send(HashOutcome::Ok { index, plen, data });
}

/// Shared: piece bytes slice + write via FdCache (thread backend).
pub(crate) fn write_job_sync(cache: &mut crate::disk::FdCache, job: &DiskWriteJob) -> Result<()> {
    let plen_usize = job.plen as usize;
    if job.data.len() < plen_usize {
        return Err(Error::Msg(format!(
            "piece {} buffer {} < plen {plen_usize}",
            job.index,
            job.data.len()
        )));
    }
    crate::disk::write_piece(cache, &job.layout, job.index, &job.data[..plen_usize])
}
