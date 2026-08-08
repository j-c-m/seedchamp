//! POSIX AIO piece-write backend (dedicated disk thread; `cfg` this OS only).
//!
//! Parity with the Linux `uring` backend:
//! - Up to `depth` **pieces** in flight
//! - All spans of a piece submitted together (`aio_write`)
//! - Completions harvested via `aio_suspend` over the global pending set
//!
//! **Critical:** the kernel keys AIO by the **address** of each `aiocb`. Control
//! blocks live in a heap `Box<[aiocb]>` so `HashMap` rehash moves only the `Box`
//! pointer, not the aiocb storage.
//!
//! **Lifetime:** never drop `job.data` / `_files` / `cbs` while any span is still
//! `EINPROGRESS` (including after `aio_cancel`).

use std::collections::{HashMap, VecDeque};
use std::os::fd::AsRawFd;
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::disk::FdCache;
use crate::error::{Error, Result};

use super::{complete_discard_job, complete_write_job, DiskWriteBackend, DiskWriteJob};

pub struct AioBackend {
    tx: SyncSender<DiskWriteJob>,
    _guard: Arc<ThreadGuard>,
}

struct ThreadGuard {
    join: std::sync::Mutex<Option<JoinHandle<()>>>,
}

impl Drop for ThreadGuard {
    fn drop(&mut self) {
        if let Ok(mut g) = self.join.lock() {
            if let Some(h) = g.take() {
                let _ = h.join();
            }
        }
    }
}

impl AioBackend {
    pub fn spawn(depth: usize, discard_writes: bool) -> Result<Self> {
        let (tx, rx) = mpsc::sync_channel::<DiskWriteJob>(depth.max(1));
        let depth = depth.max(1);
        let join = thread::Builder::new()
            .name("seedchamp-disk-aio".into())
            .spawn(move || aio_main(rx, depth, discard_writes))
            .map_err(|e| Error::Msg(format!("spawn aio disk thread: {e}")))?;
        Ok(Self {
            tx,
            _guard: Arc::new(ThreadGuard {
                join: std::sync::Mutex::new(Some(join)),
            }),
        })
    }
}

impl DiskWriteBackend for AioBackend {
    fn submit(&self, job: DiskWriteJob) -> std::result::Result<(), (Error, DiskWriteJob)> {
        self.tx
            .send(job)
            .map_err(|e| (Error::DiskWorkerStopped, e.0))
    }
}

struct PieceInFlight {
    job: DiskWriteJob,
    /// Held until all span ops complete.
    _files: Vec<std::fs::File>,
    /// Stable heap storage for aiocbs (address must not move after aio_write).
    cbs: Box<[libc::aiocb]>,
    expected: Vec<usize>,
    span_done: Vec<bool>,
    remaining: u32,
    failed: Option<Error>,
}

fn aio_main(rx: Receiver<DiskWriteJob>, depth: usize, discard_writes: bool) {
    let mut cache = FdCache::default_cache();
    let mut waiting: VecDeque<DiskWriteJob> = VecDeque::new();
    // Values are Box-backed; rehash moves Box, not aiocb heap storage.
    let mut inflight: HashMap<u64, PieceInFlight> = HashMap::new();
    let mut next_token: u64 = 1;
    let mut channel_open = true;

    while channel_open || !waiting.is_empty() || !inflight.is_empty() {
        // Fill local queue up to depth.
        while channel_open && inflight.len() + waiting.len() < depth {
            match rx.try_recv() {
                Ok(job) => waiting.push_back(job),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    channel_open = false;
                    break;
                }
            }
        }

        // Start pieces until depth in flight.
        while inflight.len() < depth {
            let Some(job) = waiting.pop_front() else {
                break;
            };
            if discard_writes {
                complete_discard_job(job);
                continue;
            }
            match start_piece(&mut cache, &mut next_token, job) {
                Ok(Some((key, piece))) => {
                    inflight.insert(key, piece);
                }
                Ok(None) => {}
                Err((job, e)) => complete_write_job(job, Err(e)),
            }
        }

        if inflight.is_empty() {
            if !channel_open {
                break;
            }
            match rx.recv() {
                Ok(job) => waiting.push_back(job),
                Err(_) => channel_open = false,
            }
            continue;
        }

        // Suspend on all incomplete span ops across all pieces.
        let list = collect_pending_ptrs(&inflight);
        if list.is_empty() {
            // Should not happen if remaining > 0; drain completions without suspend.
            harvest_completions(&mut inflight);
            continue;
        }
        let rc = unsafe {
            libc::aio_suspend(
                list.as_ptr() as *const *const libc::aiocb,
                list.len() as libc::c_int,
                std::ptr::null(),
            )
        };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() != std::io::ErrorKind::Interrupted {
                tracing::error!(error = %err, "aio_suspend fatal; draining then exiting disk thread");
                fatal_drain_and_exit(rx, waiting, inflight, err);
                return;
            }
        }

        harvest_completions(&mut inflight);
    }
    tracing::debug!("aio disk thread exit");
}

/// Cancel + wait every in-flight op, fail all jobs, then exit (thread stops).
fn fatal_drain_and_exit(
    rx: Receiver<DiskWriteJob>,
    mut waiting: VecDeque<DiskWriteJob>,
    mut inflight: HashMap<u64, PieceInFlight>,
    err: std::io::Error,
) {
    let msg = format!("aio_suspend: {err}");
    for (_, piece) in inflight.drain() {
        cancel_and_drain_piece(&piece);
        complete_write_job(piece.job, Err(Error::Msg(msg.clone())));
    }
    for job in waiting.drain(..) {
        complete_write_job(job, Err(Error::Msg(msg.clone())));
    }
    // Drain any leftover channel jobs so submitters do not hang.
    while let Ok(job) = rx.try_recv() {
        complete_write_job(job, Err(Error::Msg(msg.clone())));
    }
    // Drop rx to disconnect; further send → Error::DiskWorkerStopped.
    drop(rx);
}

/// Pointers to incomplete aiocbs — valid as long as PieceInFlight boxes live.
fn collect_pending_ptrs(inflight: &HashMap<u64, PieceInFlight>) -> Vec<*const libc::aiocb> {
    let mut list = Vec::new();
    for piece in inflight.values() {
        for (i, cb) in piece.cbs.iter().enumerate() {
            if !piece.span_done[i] {
                list.push(cb as *const libc::aiocb);
            }
        }
    }
    list
}

/// Cancel incomplete spans and wait until each leaves `EINPROGRESS`.
/// Safe to drop the piece only after this returns.
fn cancel_and_drain_piece(piece: &PieceInFlight) {
    for (i, cb) in piece.cbs.iter().enumerate() {
        if piece.span_done[i] {
            continue;
        }
        unsafe {
            let _ = libc::aio_cancel(cb.aio_fildes, cb as *const _ as *mut libc::aiocb);
        }
        drain_one_aiocb(cb as *const _ as *mut libc::aiocb);
    }
}

/// Poll until the control block is finished, then consume with `aio_return`.
fn drain_one_aiocb(cb: *mut libc::aiocb) {
    // Bound wait so a stuck kernel cannot hang the disk thread forever.
    const MAX_SPINS: u32 = 1_000_000;
    let mut spins = 0u32;
    loop {
        let err = unsafe { libc::aio_error(cb) };
        if err == libc::EINPROGRESS {
            spins = spins.saturating_add(1);
            if spins >= MAX_SPINS {
                // Still in progress after cancel — cannot free buffer safely.
                // Yield briefly and keep waiting (prefer hang over UAF).
                spins = 0;
                thread::sleep(Duration::from_millis(1));
            } else if spins % 10_000 == 0 {
                thread::yield_now();
            }
            continue;
        }
        // Finished (ok, error, or canceled) — must call aio_return to free the slot.
        let _ = unsafe { libc::aio_return(cb) };
        break;
    }
}

fn harvest_completions(inflight: &mut HashMap<u64, PieceInFlight>) {
    let keys: Vec<u64> = inflight.keys().copied().collect();
    for key in keys {
        let Some(piece) = inflight.get_mut(&key) else {
            continue;
        };
        for i in 0..piece.cbs.len() {
            if piece.span_done[i] {
                continue;
            }
            // SAFETY: cbs live in Box heap; not moved for lifetime of piece.
            let cb = &mut piece.cbs[i];
            let err = unsafe { libc::aio_error(cb) };
            if err == libc::EINPROGRESS {
                continue;
            }
            if err != 0 {
                piece.failed = Some(Error::Msg(format!(
                    "aio_error: {}",
                    std::io::Error::from_raw_os_error(err)
                )));
                // Still consume the control block.
                let _ = unsafe { libc::aio_return(cb) };
                piece.span_done[i] = true;
                piece.remaining = piece.remaining.saturating_sub(1);
                continue;
            }
            let n = unsafe { libc::aio_return(cb) };
            if n < 0 {
                piece.failed = Some(Error::Msg(format!(
                    "aio_return: {}",
                    std::io::Error::last_os_error()
                )));
            } else if n as usize != piece.expected[i] {
                piece.failed = Some(Error::Msg(format!(
                    "aio short write {}/{} span {i}",
                    n, piece.expected[i]
                )));
            }
            piece.span_done[i] = true;
            piece.remaining = piece.remaining.saturating_sub(1);
        }
        if piece.remaining == 0 {
            let piece = inflight.remove(&key).unwrap();
            let result = match piece.failed {
                Some(e) => Err(e),
                None => Ok(()),
            };
            complete_write_job(piece.job, result);
        }
    }
}

fn start_piece(
    cache: &mut FdCache,
    next_token: &mut u64,
    job: DiskWriteJob,
) -> std::result::Result<Option<(u64, PieceInFlight)>, (DiskWriteJob, Error)> {
    let plen_usize = job.plen as usize;
    if job.data.len() < plen_usize {
        let index = job.index;
        let got = job.data.len();
        return Err((
            job,
            Error::Msg(format!("piece {index} buffer {got} < plen {plen_usize}")),
        ));
    }
    let spans = match job.layout.spans_for_piece(job.index) {
        Ok(s) => s,
        Err(e) => return Err((job, e)),
    };
    if spans.is_empty() {
        complete_write_job(job, Ok(()));
        return Ok(None);
    }

    let mut files = Vec::with_capacity(spans.len());
    let mut expected = Vec::with_capacity(spans.len());
    let mut data_offs = Vec::with_capacity(spans.len());
    let mut off = 0usize;
    for span in &spans {
        let n = span.length as usize;
        let file = match cache.open_write_cloned(&span.path) {
            Ok(f) => f,
            Err(e) => return Err((job, e)),
        };
        files.push(file);
        expected.push(n);
        data_offs.push(off);
        off += n;
    }
    if off != plen_usize {
        let index = job.index;
        return Err((
            job,
            Error::Msg(format!("piece {index} span sum {off} != plen {plen_usize}")),
        ));
    }

    // Heap-stable aiocb storage (must not move after aio_write).
    let mut cbs: Box<[libc::aiocb]> = (0..spans.len())
        .map(|_| unsafe { std::mem::zeroed() })
        .collect::<Vec<_>>()
        .into_boxed_slice();

    for (i, span) in spans.iter().enumerate() {
        let n = expected[i];
        let fd = files[i].as_raw_fd();
        let cb = &mut cbs[i];
        cb.aio_fildes = fd;
        cb.aio_offset = span.file_offset as libc::off_t;
        cb.aio_buf = unsafe { job.data.as_ptr().add(data_offs[i]) as *mut libc::c_void };
        cb.aio_nbytes = n;
        cb.aio_sigevent.sigev_notify = libc::SIGEV_NONE;

        let rc = unsafe { libc::aio_write(cb) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            // Drain any already submitted spans before dropping job buffers.
            for j in 0..i {
                unsafe {
                    let prev = &mut cbs[j];
                    let _ = libc::aio_cancel(prev.aio_fildes, prev);
                    drain_one_aiocb(prev);
                }
            }
            return Err((job, Error::Msg(format!("aio_write: {err}"))));
        }
    }

    let key = *next_token;
    *next_token = next_token.wrapping_add(1).max(1);
    let remaining = spans.len() as u32;

    Ok(Some((
        key,
        PieceInFlight {
            job,
            _files: files,
            cbs,
            expected,
            span_done: vec![false; spans.len()],
            remaining,
            failed: None,
        },
    )))
}
