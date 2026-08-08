//! Linux io_uring piece-write backend (dedicated disk thread owns the ring).
//!
//! **Lifetime:** once a Write SQE is pushed/submitted, `job.data` and cloned FDs
//! must live until the matching CQE. On fatal ring errors we drain CQEs, then
//! exit the thread (fail closed). If CQEs never arrive, remaining pieces are
//! `mem::forget` so Drop cannot free kernel-referenced buffers.

use std::collections::{HashMap, VecDeque};
use std::os::fd::AsRawFd;
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use io_uring::{opcode, types, IoUring};

use crate::disk::FdCache;
use crate::error::{Error, Result};

use super::{complete_discard_job, complete_write_job, DiskWriteBackend, DiskWriteJob};

/// Ring entries: budget ~4 spans per in-flight piece depth (capped).
fn ring_entries(depth: usize) -> u32 {
    (depth.saturating_mul(4).max(16) as u32).min(4096)
}

/// `user_data` packing: high 48 bits = piece key, low 16 bits = span index.
/// Piece keys are allocated as 1,2,3… so they fit in 48 bits for practical depths.
const SPAN_IDX_MASK: u64 = 0xffff;
const PIECE_KEY_SHIFT: u32 = 16;

fn pack_user_data(piece_key: u64, span_idx: u16) -> u64 {
    (piece_key << PIECE_KEY_SHIFT) | (span_idx as u64)
}

fn unpack_user_data(ud: u64) -> (u64, u16) {
    (ud >> PIECE_KEY_SHIFT, (ud & SPAN_IDX_MASK) as u16)
}

pub struct UringBackend {
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

impl UringBackend {
    /// Probe + spawn. Fails if io_uring is unavailable.
    pub fn spawn(depth: usize, discard_writes: bool) -> Result<Self> {
        let entries = ring_entries(depth);
        let _probe =
            IoUring::new(entries).map_err(|e| Error::Msg(format!("io_uring_setup: {e}")))?;
        drop(_probe);

        let (tx, rx) = mpsc::sync_channel::<DiskWriteJob>(depth.max(1));
        let depth = depth.max(1);
        let join = thread::Builder::new()
            .name("seedchamp-disk-uring".into())
            .spawn(move || {
                if let Err(e) = uring_main(rx, depth, discard_writes, entries) {
                    tracing::error!(error = %e, "io_uring disk thread exited with error");
                }
            })
            .map_err(|e| Error::Msg(format!("spawn uring disk thread: {e}")))?;
        Ok(Self {
            tx,
            _guard: Arc::new(ThreadGuard {
                join: std::sync::Mutex::new(Some(join)),
            }),
        })
    }
}

impl DiskWriteBackend for UringBackend {
    fn submit(&self, job: DiskWriteJob) -> std::result::Result<(), (Error, DiskWriteJob)> {
        self.tx
            .send(job)
            .map_err(|e| (Error::DiskWorkerStopped, e.0))
    }
}

struct PieceInFlight {
    job: DiskWriteJob,
    /// Open files held for the lifetime of in-flight ops.
    _files: Vec<std::fs::File>,
    /// Expected write length per span (for short-write detection).
    expected: Vec<u32>,
    remaining: u32,
    failed: Option<Error>,
}

fn uring_main(
    rx: Receiver<DiskWriteJob>,
    depth: usize,
    discard_writes: bool,
    entries: u32,
) -> Result<()> {
    let mut ring = IoUring::new(entries).map_err(|e| Error::Msg(format!("io_uring_setup: {e}")))?;
    let mut cache = FdCache::default_cache();
    let mut waiting: VecDeque<DiskWriteJob> = VecDeque::new();
    let mut inflight: HashMap<u64, PieceInFlight> = HashMap::new();
    let mut next_token: u64 = 1;

    let mut channel_open = true;
    while channel_open || !waiting.is_empty() || !inflight.is_empty() {
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

        while inflight.len() < depth {
            let Some(job) = waiting.pop_front() else {
                break;
            };
            if discard_writes {
                complete_discard_job(job);
                continue;
            }
            match start_piece(&mut ring, &mut cache, &mut next_token, entries, job) {
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

        if let Err(e) = ring.submit_and_wait(1) {
            tracing::error!(error = %e, "io_uring submit_and_wait fatal; draining then exiting");
            return fatal_drain_and_exit(rx, &mut ring, waiting, inflight, e);
        }

        harvest_cqes(&mut ring, &mut inflight);
    }
    tracing::debug!("uring disk thread exit");
    Ok(())
}

fn harvest_cqes(ring: &mut IoUring, inflight: &mut HashMap<u64, PieceInFlight>) {
    let mut cq = ring.completion();
    cq.sync();
    for cqe in cq {
        let (piece_key, span_idx) = unpack_user_data(cqe.user_data());
        let res = cqe.result();
        let Some(piece) = inflight.get_mut(&piece_key) else {
            continue;
        };
        if res < 0 {
            piece.failed = Some(Error::Msg(format!("io_uring write errno {}", -res)));
        } else {
            let expected = piece.expected.get(span_idx as usize).copied().unwrap_or(0);
            if (res as u32) != expected {
                piece.failed = Some(Error::Msg(format!(
                    "io_uring short write {}/{} span {span_idx}",
                    res, expected
                )));
            }
        }
        piece.remaining = piece.remaining.saturating_sub(1);
        if piece.remaining == 0 {
            let piece = inflight.remove(&piece_key).unwrap();
            let result = match piece.failed {
                Some(e) => Err(e),
                None => Ok(()),
            };
            complete_write_job(piece.job, result);
        }
    }
}

/// Mark failed, best-effort CQE drain, fail waiting/channel, exit thread.
/// If ops never complete, `mem::forget` remaining inflight to avoid UAF on Drop.
fn fatal_drain_and_exit(
    rx: Receiver<DiskWriteJob>,
    ring: &mut IoUring,
    mut waiting: VecDeque<DiskWriteJob>,
    mut inflight: HashMap<u64, PieceInFlight>,
    err: std::io::Error,
) -> Result<()> {
    let msg = format!("io_uring submit: {err}");
    for piece in inflight.values_mut() {
        if piece.failed.is_none() {
            piece.failed = Some(Error::Msg(msg.clone()));
        }
    }

    let deadline = Instant::now() + Duration::from_secs(60);
    let mut consecutive_fatal = 0u32;
    while !inflight.is_empty() && Instant::now() < deadline {
        match ring.submit_and_wait(1) {
            Ok(_) => {
                consecutive_fatal = 0;
                harvest_cqes(ring, &mut inflight);
            }
            Err(e2) => {
                consecutive_fatal = consecutive_fatal.saturating_add(1);
                tracing::error!(error = %e2, attempt = consecutive_fatal, "io_uring drain submit_and_wait");
                // Opportunistic harvest even on error.
                harvest_cqes(ring, &mut inflight);
                if consecutive_fatal >= 8 {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
    }

    if !inflight.is_empty() {
        tracing::error!(
            leftover = inflight.len(),
            "io_uring drain incomplete; leaking piece buffers to avoid UAF"
        );
        // Do not Drop job.data / FDs while kernel may still reference them.
        std::mem::forget(inflight);
    }

    for job in waiting.drain(..) {
        complete_write_job(job, Err(Error::Msg(msg.clone())));
    }
    while let Ok(job) = rx.try_recv() {
        complete_write_job(job, Err(Error::Msg(msg.clone())));
    }
    drop(rx);

    Err(Error::Msg(msg))
}

fn start_piece(
    ring: &mut IoUring,
    cache: &mut FdCache,
    next_token: &mut u64,
    entries: u32,
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

    // Light guard: a single piece must fit in the ring.
    if spans.len() as u32 > entries {
        let index = job.index;
        let n = spans.len();
        return Err((
            job,
            Error::Msg(format!(
                "piece {index} has {n} spans > ring entries {entries}; lower multi-file span count or raise depth/ring"
            )),
        ));
    }

    let key = *next_token;
    *next_token = next_token.wrapping_add(1).max(1);

    let mut files = Vec::with_capacity(spans.len());
    let mut ops: Vec<(i32, u64, u32, usize)> = Vec::with_capacity(spans.len());
    let mut expected: Vec<u32> = Vec::with_capacity(spans.len());
    let mut off = 0usize;
    for span in &spans {
        let n = span.length as usize;
        let file = match cache.open_write_cloned(&span.path) {
            Ok(f) => f,
            Err(e) => return Err((job, e)),
        };
        let fd = file.as_raw_fd();
        files.push(file);
        expected.push(span.length as u32);
        ops.push((fd, span.file_offset, span.length as u32, off));
        off += n;
    }
    if off != plen_usize {
        let index = job.index;
        return Err((
            job,
            Error::Msg(format!("piece {index} span sum {off} != plen {plen_usize}")),
        ));
    }

    // Push/submit while job still owns data. Track how many ops were queued.
    let mut submitted = 0u32;
    for (span_idx, &(fd, file_off, len, data_off)) in ops.iter().enumerate() {
        let ptr = unsafe { job.data.as_ptr().add(data_off) };
        let entry = opcode::Write::new(types::Fd(fd), ptr, len)
            .offset(file_off)
            .build()
            .user_data(pack_user_data(key, span_idx as u16));
        // Spin until this SQE is on the SQ (or hard fail before any push for this span).
        let mut pushed = false;
        for _attempt in 0..64 {
            let push = unsafe { ring.submission().push(&entry) };
            match push {
                Ok(()) => {
                    pushed = true;
                    submitted += 1;
                    break;
                }
                Err(_) => {
                    if let Err(e) = ring.submit() {
                        if submitted == 0 {
                            return Err((job, Error::Msg(format!("io_uring submit: {e}"))));
                        }
                        // Partial: keep buffers in inflight as failed.
                        return Ok(Some((
                            key,
                            PieceInFlight {
                                job,
                                _files: files,
                                expected,
                                remaining: submitted,
                                failed: Some(Error::Msg(format!("io_uring submit: {e}"))),
                            },
                        )));
                    }
                }
            }
        }
        if !pushed {
            if submitted == 0 {
                return Err((
                    job,
                    Error::Msg("io_uring SQ full; could not push write".into()),
                ));
            }
            return Ok(Some((
                key,
                PieceInFlight {
                    job,
                    _files: files,
                    expected,
                    remaining: submitted,
                    failed: Some(Error::Msg("io_uring SQ full mid-piece".into())),
                },
            )));
        }
    }

    if let Err(e) = ring.submit() {
        if submitted == 0 {
            return Err((job, Error::Msg(format!("io_uring submit: {e}"))));
        }
        // Ops may already be in the SQ from earlier submit() calls during push.
        return Ok(Some((
            key,
            PieceInFlight {
                job,
                _files: files,
                expected,
                remaining: submitted,
                failed: Some(Error::Msg(format!("io_uring submit: {e}"))),
            },
        )));
    }

    Ok(Some((
        key,
        PieceInFlight {
            job,
            _files: files,
            expected,
            remaining: submitted,
            failed: None,
        },
    )))
}
