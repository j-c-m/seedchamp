//! Peer worker pool: Compio runtimes.
//!
//! Topology:
//! - **1 accept thread** (`seedchamp-acc`): Compio runtime; TCP accept loop and light
//!   session service tasks (tick / bootstrap). Announce fan-out uses the tracker
//!   thread, not accept.
//! - **1 tracker thread** (`seedchamp-trk`): Compio runtime; HTTP(S) announce via
//!   cyper, UDP announce via Compio `UdpSocket`, stopped/quit announces.
//! - **N peer workers** (`seedchamp-io`): Compio runtimes; peer sessions pinned after
//!   **least-peers** assignment (inbound handoff or outbound dial).
//!
//! Peer wire, accept, tracker, and session tick/bootstrap use Compio (**K18** /
//! **K19**). Blocking SQLite/disk work uses [`PeerWorkerPool::run_blocking`].
//! Each peer worker thread shares one seed-fill [`crate::disk::FdCache`] via
//! thread-local storage (Compio files are `!Send`; peers are pinned per worker).
//!
//! Shutdown: control plane calls [`PeerWorkerPool::shutdown`] (never drop runtimes
//! from an async worker).

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle as ThreadJoin;
use std::time::{Duration, Instant};

use compio::runtime::Runtime;
use futures_channel::oneshot;

use crate::error::{Error, Result};

/// Default peer worker count = available parallelism (at least 1).
pub fn default_peer_workers() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .max(1)
}

/// 15-char thread names (Linux TASK_COMM_LEN / FreeBSD tdname).
const NAME_IO: &str = "seedchamp-io";
const NAME_ACC: &str = "seedchamp-acc";
const NAME_TRK: &str = "seedchamp-trk";

type Spawning = Box<dyn Spawnable + Send>;

trait Spawnable {
    fn spawn(self: Box<Self>, rt: &Runtime);
}

struct Job<F, R> {
    func: F,
    done: Option<oneshot::Sender<R>>,
}

impl<F, Fut, R> Spawnable for Job<F, R>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = R> + 'static,
    R: Send + 'static,
{
    fn spawn(self: Box<Self>, rt: &Runtime) {
        let Job { func, done } = *self;
        let fut = func();
        rt.spawn(async move {
            let out = fut.await;
            if let Some(tx) = done {
                let _ = tx.send(out);
            }
        })
        .detach();
    }
}

struct WorkerSlot {
    tx: flume::Sender<Option<Spawning>>,
    /// Live peer sessions assigned to this worker (least-peers input).
    peer_count: Arc<AtomicUsize>,
    thread: Mutex<Option<ThreadJoin<()>>>,
}

/// Compio peer I/O pool: accept + tracker + N peer workers.
pub struct PeerWorkerPool {
    workers: Vec<WorkerSlot>,
    accept_tx: flume::Sender<Option<Spawning>>,
    accept_thread: Mutex<Option<ThreadJoin<()>>>,
    tracker_tx: flume::Sender<Option<Spawning>>,
    tracker_thread: Mutex<Option<ThreadJoin<()>>>,
    n_workers: usize,
    /// Process-wide live peer tasks (sum of per-worker counts).
    pub active_peers: Arc<AtomicUsize>,
    shut: AtomicBool,
}

impl PeerWorkerPool {
    /// Build pool: `workers` peer Compio threads + 1 accept + 1 tracker.
    pub fn new(workers: usize) -> Result<Self> {
        let n_workers = workers.max(1);

        let mut slots = Vec::with_capacity(n_workers);
        for i in 0..n_workers {
            let (tx, rx) = flume::unbounded::<Option<Spawning>>();
            let peer_count = Arc::new(AtomicUsize::new(0));
            let name = if n_workers == 1 {
                NAME_IO.to_string()
            } else {
                // Prefer stable prefix for Status grouping; index in full name when short.
                format!("{NAME_IO}-{i}")
            };
            let thread = std::thread::Builder::new()
                .name(name)
                .spawn(move || worker_main(rx))
                .map_err(|e| Error::Msg(format!("peer worker thread: {e}")))?;
            slots.push(WorkerSlot {
                tx,
                peer_count,
                thread: Mutex::new(Some(thread)),
            });
        }

        let (accept_tx, accept_rx) = flume::unbounded::<Option<Spawning>>();
        let accept_thread = std::thread::Builder::new()
            .name(NAME_ACC.into())
            .spawn(move || worker_main(accept_rx))
            .map_err(|e| Error::Msg(format!("accept thread: {e}")))?;

        let (tracker_tx, tracker_rx) = flume::unbounded::<Option<Spawning>>();
        let tracker_thread = std::thread::Builder::new()
            .name(NAME_TRK.into())
            .spawn(move || worker_main(tracker_rx))
            .map_err(|e| Error::Msg(format!("tracker thread: {e}")))?;

        Ok(Self {
            workers: slots,
            accept_tx,
            accept_thread: Mutex::new(Some(accept_thread)),
            tracker_tx,
            tracker_thread: Mutex::new(Some(tracker_thread)),
            n_workers,
            active_peers: Arc::new(AtomicUsize::new(0)),
            shut: AtomicBool::new(false),
        })
    }

    pub fn workers(&self) -> usize {
        self.n_workers
    }

    /// Index of the worker with the fewest assigned peers (stable tie-break: lowest index).
    pub fn least_peers_worker(&self) -> usize {
        let mut best_i = 0usize;
        let mut best_n = usize::MAX;
        for (i, w) in self.workers.iter().enumerate() {
            let n = w.peer_count.load(Ordering::Relaxed);
            if n < best_n {
                best_n = n;
                best_i = i;
            }
        }
        best_i
    }

    /// Per-worker peer counts (tests).
    #[cfg(test)]
    pub fn peer_counts(&self) -> Vec<usize> {
        self.workers
            .iter()
            .map(|w| w.peer_count.load(Ordering::Relaxed))
            .collect()
    }

    /// Run a task on the **accept** Compio runtime (accept loop, tick, bootstrap).
    pub fn spawn_accept<Fn, Fut, R>(&self, f: Fn) -> Result<oneshot::Receiver<R>>
    where
        Fn: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = R> + 'static,
        R: Send + 'static,
    {
        self.send(&self.accept_tx, f)
    }

    /// Run a task on the **tracker** Compio runtime (HTTP announce, quit stopped).
    pub fn spawn_tracker<Fn, Fut, R>(&self, f: Fn) -> Result<oneshot::Receiver<R>>
    where
        Fn: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = R> + 'static,
        R: Send + 'static,
    {
        self.send(&self.tracker_tx, f)
    }

    /// Run a **peer session** (or dial) on the least-loaded peer worker.
    ///
    /// Increments that worker’s peer count for the lifetime of the future.
    /// Seed fill uses that thread’s TLS [`crate::disk::FdCache`] (shared by all
    /// peers on the worker).
    pub fn spawn_peer<Fn, Fut, R>(&self, f: Fn) -> Result<oneshot::Receiver<R>>
    where
        Fn: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = R> + 'static,
        R: Send + 'static,
    {
        if self.shut.load(Ordering::SeqCst) {
            return Err(Error::Msg("peer pool shut down".into()));
        }
        let i = self.least_peers_worker();
        let count = Arc::clone(&self.workers[i].peer_count);
        let global = Arc::clone(&self.active_peers);
        count.fetch_add(1, Ordering::Relaxed);
        global.fetch_add(1, Ordering::Relaxed);
        let tx = self.workers[i].tx.clone();
        let (done_tx, done_rx) = oneshot::channel();
        let job = Job {
            func: move || {
                let fut = f();
                async move {
                    struct Guard {
                        count: Arc<AtomicUsize>,
                        global: Arc<AtomicUsize>,
                    }
                    impl Drop for Guard {
                        fn drop(&mut self) {
                            self.count.fetch_sub(1, Ordering::Relaxed);
                            self.global.fetch_sub(1, Ordering::Relaxed);
                        }
                    }
                    let _g = Guard { count, global };
                    fut.await
                }
            },
            done: Some(done_tx),
        };
        tx.send(Some(Box::new(job)))
            .map_err(|_| Error::Msg("peer worker queue closed".into()))?;
        Ok(done_rx)
    }

    /// Service task on accept runtime (tick, session bootstrap — not announce).
    ///
    /// Prefer this for non-peer work so peer workers stay free for sockets.
    /// Tracker announces use [`Self::spawn_tracker`].
    pub fn spawn<Fn, Fut, R>(&self, f: Fn) -> Result<oneshot::Receiver<R>>
    where
        Fn: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = R> + 'static,
        R: Send + 'static,
    {
        self.spawn_accept(f)
    }

    /// Drive `fut` on the accept runtime and wait (control-plane path).
    pub fn block_on<F>(&self, fut: F) -> F::Output
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        if let Err(e) = self.spawn_accept(move || async move {
            let out = fut.await;
            let _ = tx.send(out);
        }) {
            panic!("peer pool block_on: {e}");
        }
        futures::executor::block_on(rx)
            .unwrap_or_else(|_| panic!("peer pool accept runtime stopped during block_on"))
    }

    /// Drive a factory on the tracker runtime and wait (quit stopped-announce).
    ///
    /// Takes `FnOnce() -> Fut` so the future is **created on the tracker thread**.
    /// Required because cyper / Compio timer futures are `!Send`.
    pub fn block_on_tracker<Fn, Fut, R>(&self, f: Fn) -> R
    where
        Fn: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = R> + 'static,
        R: Send + 'static,
    {
        let rx = match self.spawn_tracker(f) {
            Ok(rx) => rx,
            Err(e) => panic!("peer pool block_on_tracker: {e}"),
        };
        futures::executor::block_on(rx)
            .unwrap_or_else(|_| panic!("peer pool tracker runtime stopped during block_on_tracker"))
    }

    /// Blocking work on Compio’s asyncify pool (catalog SQLite, etc.).
    ///
    /// Must be called from a Compio runtime task (accept or peer worker).
    pub async fn run_blocking<T: Send + 'static>(
        f: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T> {
        compio::runtime::spawn_blocking(f)
            .await
            .map_err(|e| Error::Msg(format!("spawn_blocking: {e}")))
    }

    /// Shut down accept + peer workers. Safe to call multiple times.
    pub fn shutdown(&self) {
        if self
            .shut
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        let _ = self.accept_tx.send(None);
        let _ = self.tracker_tx.send(None);
        for w in &self.workers {
            let _ = w.tx.send(None);
        }
        // Join with 2s wall budget; do not hang control plane on stuck workers.
        let deadline = Instant::now() + Duration::from_secs(2);
        if let Some(t) = self
            .accept_thread
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            join_until(t, deadline);
        }
        if let Some(t) = self
            .tracker_thread
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            join_until(t, deadline);
        }
        for w in &self.workers {
            if let Some(t) = w.thread.lock().unwrap_or_else(|e| e.into_inner()).take() {
                join_until(t, deadline);
            }
        }
    }

    fn send<Fn, Fut, R>(
        &self,
        tx: &flume::Sender<Option<Spawning>>,
        f: Fn,
    ) -> Result<oneshot::Receiver<R>>
    where
        Fn: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = R> + 'static,
        R: Send + 'static,
    {
        if self.shut.load(Ordering::SeqCst) {
            return Err(Error::Msg("peer pool shut down".into()));
        }
        let (done_tx, done_rx) = oneshot::channel();
        let job = Job {
            func: f,
            done: Some(done_tx),
        };
        tx.send(Some(Box::new(job)))
            .map_err(|_| Error::Msg("runtime queue closed".into()))?;
        Ok(done_rx)
    }
}

impl Drop for PeerWorkerPool {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn worker_main(rx: flume::Receiver<Option<Spawning>>) {
    let rt = match Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!(error = %e, "compio Runtime::new failed");
            return;
        }
    };
    rt.block_on(async move {
        while let Ok(msg) = rx.recv_async().await {
            match msg {
                None => break,
                Some(job) => {
                    Runtime::with_current(|rt| job.spawn(rt));
                }
            }
        }
    });
}

/// Join `t` until `deadline`. `JoinHandle` has no timed join: a helper thread
/// owns `join` while we `recv_timeout`. On timeout the worker may still run
/// (helper joins later / process exit reaps); control plane continues.
fn join_until(t: ThreadJoin<()>, deadline: Instant) {
    let remain = deadline.saturating_duration_since(Instant::now());
    if remain.is_zero() {
        std::mem::forget(t);
        return;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = t.join();
        let _ = tx.send(());
    });
    let _ = rx.recv_timeout(remain);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;

    #[test]
    fn least_peers_picks_empty_worker() {
        let pool = PeerWorkerPool::new(3).expect("pool");
        assert_eq!(pool.least_peers_worker(), 0);
        // Simulate load on 0 and 1.
        pool.workers[0].peer_count.store(2, Ordering::Relaxed);
        pool.workers[1].peer_count.store(1, Ordering::Relaxed);
        pool.workers[2].peer_count.store(0, Ordering::Relaxed);
        assert_eq!(pool.least_peers_worker(), 2);
        pool.workers[2].peer_count.store(5, Ordering::Relaxed);
        assert_eq!(pool.least_peers_worker(), 1);
        pool.shutdown();
    }

    #[test]
    fn spawn_peer_increments_and_decrements() {
        let pool = PeerWorkerPool::new(2).expect("pool");
        let barrier = Arc::new(Barrier::new(2));
        let b2 = Arc::clone(&barrier);
        let rx = pool
            .spawn_peer(move || async move {
                b2.wait();
                7i32
            })
            .expect("spawn_peer");
        // Peer count should be 1 on some worker while task runs.
        std::thread::sleep(Duration::from_millis(50));
        let sum: usize = pool.peer_counts().iter().sum();
        assert_eq!(sum, 1);
        assert_eq!(pool.active_peers.load(Ordering::Relaxed), 1);
        barrier.wait();
        assert_eq!(futures::executor::block_on(rx).unwrap(), 7);
        std::thread::sleep(Duration::from_millis(50));
        let sum: usize = pool.peer_counts().iter().sum();
        assert_eq!(sum, 0);
        assert_eq!(pool.active_peers.load(Ordering::Relaxed), 0);
        pool.shutdown();
    }

    #[test]
    fn spawn_accept_runs() {
        let pool = PeerWorkerPool::new(1).expect("pool");
        let rx = pool.spawn_accept(|| async { 42u32 }).expect("spawn_accept");
        assert_eq!(futures::executor::block_on(rx).unwrap(), 42);
        pool.shutdown();
    }

    #[test]
    fn spawn_tracker_runs() {
        let pool = PeerWorkerPool::new(1).expect("pool");
        let rx = pool
            .spawn_tracker(|| async { 7u32 })
            .expect("spawn_tracker");
        assert_eq!(futures::executor::block_on(rx).unwrap(), 7);
        pool.shutdown();
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn thread_names_accept_tracker_and_io() {
        fn counts() -> std::collections::HashMap<String, usize> {
            let mut map = std::collections::HashMap::new();
            let Ok(rd) = std::fs::read_dir("/proc/self/task") else {
                return map;
            };
            for ent in rd.flatten() {
                let name = std::fs::read_to_string(ent.path().join("comm"))
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                if !name.is_empty() {
                    *map.entry(name).or_default() += 1;
                }
            }
            map
        }
        let n = 2usize;
        let pool = PeerWorkerPool::new(n).expect("pool");
        // Parallel tests share /proc; only require our names to appear while pool is live.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let after = counts();
            let acc = after.get(NAME_ACC).copied().unwrap_or(0);
            let trk = after.get(NAME_TRK).copied().unwrap_or(0);
            let io_like = after
                .iter()
                .filter(|(k, _)| k.starts_with("seedchamp-io"))
                .map(|(_, v)| *v)
                .sum::<usize>();
            if acc >= 1 && trk >= 1 && io_like >= n {
                break;
            }
            assert!(
                deadline > std::time::Instant::now(),
                "expected seedchamp-acc + seedchamp-trk + {n} seedchamp-io*; got {after:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        pool.shutdown();
    }
}
