//! Piece-hash worker pool.
//!
//! **N OS threads** (default = `available_parallelism`) share multi-consumer
//! queues ([`crossbeam_channel`]).
//!
//! Jobs:
//! - **Leech verify (high priority):** SHA-1 on an in-RAM piece → on success
//!   hand off to [`DiskWorker`].
//! - **Recheck (low priority):** windowed disk read + SHA-1 → recheck
//!   orchestrator. Workers **always drain leech work before recheck**.
//!
//! Peer I/O never runs here.

use std::sync::Arc;
use std::thread::{self, JoinHandle};

use crossbeam_channel::{unbounded, Receiver, Sender, TryRecvError};
use flume::Sender as FlumeSender;
use parking_lot::Mutex;
use sha1::{Digest, Sha1};

use crate::disk::{hash_piece_windowed, FdCache, StorageLayout};
use crate::error::{Error, Result};

use super::disk_worker::{DiskWorker, DiskWriteJob};

/// Result delivered back to the leecher that submitted the job.
///
/// `data` is the staging slot buffer (fixed `piece_length`); the peer must
/// [`crate::staging::StagingPool::reclaim`] it so the slot returns to Free.
#[derive(Debug)]
pub enum HashOutcome {
    /// SHA-1 matched **and** piece written to disk (or discarded when
    /// [`super::disk_worker::DiskWorker::discard_writes`]).
    Ok {
        index: u32,
        plen: u32,
        data: Vec<u8>,
    },
    /// SHA-1 mismatch — caller should reclaim and re-download.
    HashFail {
        index: u32,
        plen: u32,
        data: Vec<u8>,
    },
    /// Durable write / disk submit failed — reclaim and re-download (v1).
    /// Future: may re-queue verified `data` without re-fetch.
    WriteFail {
        index: u32,
        plen: u32,
        data: Vec<u8>,
    },
}

/// One piece to verify for leech (data already in RAM).
#[derive(Debug)]
pub struct HashJob {
    pub index: u32,
    pub plen: u32,
    pub data: Vec<u8>,
    pub expected: Vec<u8>,
    /// Shared layout — never clone multi‑file path lists per piece.
    pub layout: Arc<StorageLayout>,
    pub reply: FlumeSender<HashOutcome>,
}

/// One piece to check during full-torrent recheck (worker reads disk).
pub struct RecheckJob {
    pub index: u32,
    pub expected: [u8; 20],
    pub layout: Arc<StorageLayout>,
    pub reply: Sender<RecheckPieceResult>,
}

/// Per-piece recheck outcome (no disk write).
#[derive(Debug, Clone, Copy)]
pub enum RecheckPieceResult {
    Good {
        index: u32,
    },
    Bad {
        index: u32,
    },
    /// Unreadable / missing data.
    Missing {
        index: u32,
    },
}

/// Default hash worker count = hardware concurrency (at least 1).
pub fn default_hash_workers() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .max(1)
}

/// Cloneable handle to the session hash worker pool.
#[derive(Clone)]
pub struct HashPool {
    leech_tx: Sender<HashJob>,
    recheck_tx: Sender<RecheckJob>,
    workers: usize,
    _threads: Arc<HashThreadGuard>,
}

struct HashThreadGuard {
    joins: Mutex<Vec<JoinHandle<()>>>,
}

impl Drop for HashThreadGuard {
    fn drop(&mut self) {
        // Dropping all Senders ends the workers (channel disconnect).
        let joins = std::mem::take(&mut *self.joins.lock());
        for h in joins {
            let _ = h.join();
        }
    }
}

impl HashPool {
    /// Spawn N hash workers. Verified leech pieces go to `disk`.
    pub fn spawn_n(disk: Arc<DiskWorker>, n: usize) -> Result<Self> {
        let n = n.max(1);
        // Separate queues: leech drained before recheck.
        let (leech_tx, leech_rx) = unbounded::<HashJob>();
        let (recheck_tx, recheck_rx) = unbounded::<RecheckJob>();
        let mut joins = Vec::with_capacity(n);
        for i in 0..n {
            let leech_rx = leech_rx.clone();
            let recheck_rx = recheck_rx.clone();
            let disk = disk.clone();
            let join = thread::Builder::new()
                .name(format!("seedchamp-hash-{i}"))
                .spawn(move || hash_worker_main(leech_rx, recheck_rx, disk))
                .map_err(|e| Error::Msg(format!("spawn hash worker {i}: {e}")))?;
            joins.push(join);
        }
        Ok(Self {
            leech_tx,
            recheck_tx,
            workers: n,
            _threads: Arc::new(HashThreadGuard {
                joins: Mutex::new(joins),
            }),
        })
    }

    pub fn workers(&self) -> usize {
        self.workers
    }

    /// Queue a leech piece for SHA-1 (+ disk write on success). High priority.
    ///
    /// On channel disconnect, returns the job so the caller can reclaim `data`.
    #[allow(clippy::result_large_err)] // intentional: return HashJob for buffer reclaim
    pub fn submit(&self, job: HashJob) -> std::result::Result<(), (Error, HashJob)> {
        self.leech_tx
            .send(job)
            .map_err(|e| (Error::Msg("hash workers stopped".into()), e.0))
    }

    /// Queue a recheck piece (worker does windowed pread + SHA-1). Low priority.
    pub fn submit_recheck(&self, job: RecheckJob) -> Result<()> {
        self.recheck_tx
            .send(job)
            .map_err(|_| Error::Msg("hash workers stopped".into()))
    }
}

/// Drain leech queue before recheck; never take recheck while leech is queued.
fn hash_worker_main(
    leech_rx: Receiver<HashJob>,
    recheck_rx: Receiver<RecheckJob>,
    disk: Arc<DiskWorker>,
) {
    let mut recheck_cache = FdCache::default_cache();
    loop {
        // 1) Always empty the high-priority leech queue first.
        while let Ok(job) = leech_rx.try_recv() {
            handle_verify(job, &disk);
        }

        // 2) One low-priority recheck if present (then loop to re-check leech).
        match recheck_rx.try_recv() {
            Ok(job) => {
                handle_recheck(job, &mut recheck_cache);
                continue;
            }
            Err(TryRecvError::Disconnected) => {
                // Recheck side closed: only leech remains.
                match leech_rx.recv() {
                    Ok(job) => handle_verify(job, &disk),
                    Err(_) => return,
                }
            }
            Err(TryRecvError::Empty) => {
                // 3) Idle: block until either queue has work.
                crossbeam_channel::select! {
                    recv(leech_rx) -> msg => {
                        match msg {
                            Ok(job) => handle_verify(job, &disk),
                            Err(_) => {
                                // Leech closed — finish recheck then exit.
                                while let Ok(job) = recheck_rx.recv() {
                                    handle_recheck(job, &mut recheck_cache);
                                }
                                return;
                            }
                        }
                    }
                    recv(recheck_rx) -> msg => {
                        match msg {
                            Ok(job) => {
                                // Another leech may have arrived; handle it first.
                                if let Ok(lj) = leech_rx.try_recv() {
                                    handle_verify(lj, &disk);
                                    // Still do this recheck after (leech drained next loop).
                                    handle_recheck(job, &mut recheck_cache);
                                } else {
                                    handle_recheck(job, &mut recheck_cache);
                                }
                            }
                            Err(_) => {
                                // Recheck closed — drain leech only.
                                while let Ok(job) = leech_rx.recv() {
                                    handle_verify(job, &disk);
                                }
                                return;
                            }
                        }
                    }
                }
            }
        }
    }
}

fn handle_verify(job: HashJob, disk: &DiskWorker) {
    let plen_usize = job.plen as usize;
    // Slot buffers are full piece_length; only the first `plen` bytes are the piece.
    let piece_bytes = if job.data.len() >= plen_usize {
        &job.data[..plen_usize]
    } else {
        job.data.as_slice()
    };
    if !sha1_eq(piece_bytes, &job.expected) {
        let _ = job.reply.send(HashOutcome::HashFail {
            index: job.index,
            plen: job.plen,
            data: job.data,
        });
        return;
    }
    let HashJob {
        index,
        plen,
        data,
        layout,
        reply,
        ..
    } = job;
    // Bench: skip disk hop entirely when discard is on (no pwrite, no queue delay).
    if disk.discard_writes() {
        let _ = reply.send(HashOutcome::Ok { index, plen, data });
        return;
    }
    match disk.submit_write(DiskWriteJob {
        index,
        plen,
        data,
        layout,
        reply: reply.clone(),
    }) {
        Ok(()) => {}
        Err((e, write)) => {
            tracing::error!(error = %e, piece = index, "disk submit failed");
            let _ = reply.send(HashOutcome::WriteFail {
                index: write.index,
                plen: write.plen,
                data: write.data,
            });
        }
    }
}

fn handle_recheck(job: RecheckJob, cache: &mut FdCache) {
    let mut hasher = Sha1::new();
    match hash_piece_windowed(cache, &job.layout, job.index, &mut hasher) {
        Ok(()) => {
            let digest = hasher.finalize();
            let r = if digest.as_slice() == job.expected.as_slice() {
                RecheckPieceResult::Good { index: job.index }
            } else {
                RecheckPieceResult::Bad { index: job.index }
            };
            let _ = job.reply.send(r);
        }
        Err(e) => {
            tracing::debug!(piece = job.index, error = %e, "recheck piece unreadable");
            let _ = job
                .reply
                .send(RecheckPieceResult::Missing { index: job.index });
        }
    }
}

fn sha1_eq(data: &[u8], expected20: &[u8]) -> bool {
    if expected20.len() != 20 {
        return false;
    }
    let mut h = Sha1::new();
    h.update(data);
    h.finalize().as_slice() == expected20
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk::spans::FileLayout;
    use crate::disk::{ensure_storage, read_piece, FdCache};
    use crate::runtime::DEFAULT_DISK_DEPTH;
    use crate::staging::BLOCK_SIZE;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    fn one_piece_layout(dir: &std::path::Path, len: usize) -> StorageLayout {
        StorageLayout {
            data_root: dir.to_path_buf(),
            piece_length: len as u32,
            piece_count: 1,
            total_size: len as u64,
            files: vec![FileLayout {
                path: PathBuf::from("x"),
                size: len as u64,
                offset: 0,
                priority: 1,
            }],
        }
    }

    #[test]
    fn hash_pool_discard_writes_skips_pwrite() {
        let dir = tempfile::tempdir().unwrap();
        let len = BLOCK_SIZE as usize + 50;
        let data: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        let mut h = Sha1::new();
        h.update(&data);
        let digest = h.finalize().to_vec();

        let layout = one_piece_layout(dir.path(), len);
        // No ensure_storage — discard must not need the file.

        let disk =
            Arc::new(DiskWorker::spawn_with_options(true, "thread", DEFAULT_DISK_DEPTH).unwrap());
        assert!(disk.discard_writes());
        let pool = HashPool::spawn_n(disk, 2).unwrap();
        let layout = Arc::new(layout);
        let (tx, rx) = flume::unbounded();
        pool.submit(HashJob {
            index: 0,
            plen: len as u32,
            data: data.clone(),
            expected: digest,
            layout: Arc::clone(&layout),
            reply: tx,
        })
        .unwrap();
        match rx.recv().unwrap() {
            HashOutcome::Ok { data: out, .. } => assert_eq!(out.len(), len),
            HashOutcome::HashFail { .. } | HashOutcome::WriteFail { .. } => {
                panic!("expected ok under discard_writes")
            }
        }
        // File never created.
        assert!(!dir.path().join("x").exists());
    }

    #[test]
    fn hash_pool_parallel_ok_and_fail() {
        let dir = tempfile::tempdir().unwrap();
        let len = BLOCK_SIZE as usize + 100;
        let data: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        let mut h = Sha1::new();
        h.update(&data);
        let digest = h.finalize().to_vec();

        let layout = one_piece_layout(dir.path(), len);
        ensure_storage(&layout).unwrap();

        let disk =
            Arc::new(DiskWorker::spawn_with_options(false, "thread", DEFAULT_DISK_DEPTH).unwrap());
        let pool = HashPool::spawn_n(disk, 4).unwrap();
        assert_eq!(pool.workers(), 4);

        let layout = Arc::new(layout);
        let (tx, rx) = flume::unbounded();
        for _ in 0..8 {
            pool.submit(HashJob {
                index: 0,
                plen: len as u32,
                data: data.clone(),
                expected: digest.clone(),
                layout: Arc::clone(&layout),
                reply: tx.clone(),
            })
            .unwrap();
        }
        for _ in 0..8 {
            match rx.recv().unwrap() {
                HashOutcome::Ok { data, .. } => {
                    assert_eq!(data.len(), len);
                }
                HashOutcome::HashFail { .. } | HashOutcome::WriteFail { .. } => {
                    panic!("expected ok")
                }
            }
        }
        let mut cache = FdCache::default_cache();
        let mut out = Vec::new();
        read_piece(&mut cache, &layout, 0, &mut out).unwrap();
        assert_eq!(out, data);

        let mut bad = data;
        bad[0] ^= 0xff;
        pool.submit(HashJob {
            index: 0,
            plen: len as u32,
            data: bad,
            expected: digest,
            layout,
            reply: tx,
        })
        .unwrap();
        match rx.recv().unwrap() {
            HashOutcome::HashFail { index, data, .. } => {
                assert_eq!(index, 0);
                assert_eq!(data.len(), len);
            }
            HashOutcome::Ok { .. } | HashOutcome::WriteFail { .. } => {
                panic!("expected HashFail")
            }
        }
    }

    /// Single worker: a leech job submitted after recheck jobs must still finish
    /// without waiting for all rechecks (priority drain).
    #[test]
    fn leech_priority_over_recheck() {
        let dir = tempfile::tempdir().unwrap();
        let len = BLOCK_SIZE as usize;
        let data: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        let mut h = Sha1::new();
        h.update(&data);
        let digest = h.finalize();
        let mut expected = [0u8; 20];
        expected.copy_from_slice(digest.as_slice());

        let layout = one_piece_layout(dir.path(), len);
        ensure_storage(&layout).unwrap();
        // Write file so recheck can read (will still be slow-ish with many jobs).
        std::fs::write(dir.path().join("x"), &data).unwrap();

        let disk =
            Arc::new(DiskWorker::spawn_with_options(false, "thread", DEFAULT_DISK_DEPTH).unwrap());
        let pool = HashPool::spawn_n(disk, 1).unwrap();
        let layout = Arc::new(layout);

        let (rtx, rrx) = unbounded::<RecheckPieceResult>();
        // Queue many rechecks first.
        for _ in 0..32u32 {
            pool.submit_recheck(RecheckJob {
                index: 0,
                expected,
                layout: Arc::clone(&layout),
                reply: rtx.clone(),
            })
            .unwrap();
        }

        let (ltx, lrx) = flume::unbounded();
        let leech_done = Arc::new(AtomicUsize::new(0));
        let flag = leech_done.clone();
        // Submit leech after rechecks are queued.
        pool.submit(HashJob {
            index: 0,
            plen: len as u32,
            data: data.clone(),
            expected: expected.to_vec(),
            layout: Arc::clone(&layout),
            reply: ltx,
        })
        .unwrap();

        // Leech should complete quickly even with recheck backlog.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(HashOutcome::Ok { .. }) = lrx.try_recv() {
                flag.store(1, Ordering::SeqCst);
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!("leech hash did not complete within 5s under recheck load");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(leech_done.load(Ordering::SeqCst), 1);

        // Drain recheck results so workers can exit cleanly.
        drop(rtx);
        let mut n = 0;
        while rrx.recv_timeout(Duration::from_secs(10)).is_ok() {
            n += 1;
            if n >= 32 {
                break;
            }
        }
        assert!(n >= 1);
    }
}
