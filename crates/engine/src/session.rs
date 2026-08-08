//! Unified session runtime: start/stop torrents; auto leech → seed.
//!
//! **I/O model (design-locked):** Compio accept thread + tracker thread + N peer-worker
//! OS threads (default = CPU count). Peer sessions are Compio tasks pinned after
//! least-peers. Tick / bootstrap are Compio tasks on accept. **K18** Compio only.
//! **K19** Compio completion I/O.
//!
//! Split modules: [`config`], [`snapshot`], [`rates`], [`announce`], [`accept`], [`dial`].

mod accept;
mod announce;
mod catalog_io;
mod config;
mod dial;
mod dial_policy;
mod lifecycle;
mod limits;
mod peer_policy;
mod rates;
mod relocate;
mod snapshot;

#[cfg(test)]
mod tests;

use crate::rate_limit::WireRateLimiter;
pub use config::RuntimeConfig;
pub use relocate::{RelocateKind, RelocateReport};
pub use snapshot::{
    set_peer_crypto, PeerCrypto, PeerDirection, PeerInfo, SessionSnapshot, TorrentLive,
};

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};

use crate::catalog::Catalog;
use crate::error::Result;
use crate::tracker::HostLimiter;

use crate::hot::HotRegistry;
use crate::library::generate_peer_id_with_prefix;
use crate::runtime::DiskWorker;
use crate::runtime::{default_hash_workers, HashPool};
use crate::runtime::{default_peer_workers, PeerWorkerPool};

use announce::{announce_for, AnnounceBaseline, TorrentAnnounce};
use dial_policy::DialData;
use rates::RateSample;

/// Live peer bookkeeping (shared with accept / dial paths in this module tree).
struct LivePeer {
    id: u64,
    torrent_id: i64,
    torrent_name: String,
    addr: SocketAddr,
    direction: PeerDirection,
    /// Preferred: shared atomics updated from wire I/O.
    wire_up: Option<Arc<AtomicU64>>,
    wire_down: Option<Arc<AtomicU64>>,
    /// Fallback atomics when no shared wire counters.
    uploaded: AtomicU64,
    downloaded: AtomicU64,
    connected_at: Instant,
    /// Set true when torrent is stopped so the peer task exits.
    cancel: Arc<AtomicBool>,
    /// Dropped on cancel so duplex idle parks wake (no poller).
    stop_tx: Mutex<Option<flume::Sender<()>>>,
    /// Leecher request queue (outstanding / adaptive target).
    queue_outstanding: Arc<AtomicU64>,
    queue_target: Arc<AtomicU64>,
    /// Peer is interested in downloading from us.
    peer_interested: Arc<AtomicBool>,
    /// Remote is choking us.
    peer_choking: Arc<AtomicBool>,
    /// Local Interested flag for this peer.
    am_interested: Arc<AtomicBool>,
    /// In-flight upload Request count for this peer.
    upload_pending: Arc<AtomicU64>,
    /// Remote peer have-count (bitfield + HAVE).
    peer_have: Arc<AtomicU32>,
    /// Torrent piece count for % (0 until bound).
    piece_count: Arc<AtomicU32>,
    /// Negotiated wire encryption ([`PeerCrypto`] as u8).
    crypto: Arc<AtomicU8>,
    /// Remote client label (peer_id → LTEP `v` when available).
    client_label: Arc<Mutex<String>>,
    /// Tracker listen port when known (outbound dest port). Inbound: None.
    listen_port: Option<u16>,
}

impl LivePeer {
    /// Mark cancelled and drop stop-wake sender (wakes duplex parks).
    fn signal_cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
        let _ = self.stop_tx.lock().take();
    }

    fn up(&self) -> u64 {
        self.wire_up
            .as_ref()
            .map(|a| a.load(Ordering::Relaxed))
            .unwrap_or_else(|| self.uploaded.load(Ordering::Relaxed))
    }
    fn down(&self) -> u64 {
        self.wire_down
            .as_ref()
            .map(|a| a.load(Ordering::Relaxed))
            .unwrap_or_else(|| self.downloaded.load(Ordering::Relaxed))
    }
}

struct TorrentBytes {
    up: AtomicU64,
    down: AtomicU64,
}

/// Coalesced piece-have events for catalog bitfield durability.
///
/// RAM [`HotTorrent::mark_have`] is the live source of truth. Catalog writes are
/// throttled to [`PIECE_HAVE_FLUSH_INTERVAL`] and forced on complete / stop / exit.
struct PieceHaveBuffer {
    pending: Mutex<Vec<(i64, u32, u32)>>,
    last_flush: Mutex<Instant>,
}

/// How often incomplete bitfields are checkpointed to SQLite while leeching.
pub(super) const PIECE_HAVE_FLUSH_INTERVAL: Duration = Duration::from_secs(10);

/// Session shared state (visible to `accept` and other session submodules).
struct Inner {
    db: PathBuf,
    cfg: RuntimeConfig,
    peer_id: [u8; 20],
    stop: AtomicBool,
    /// Dropped on [`SessionRuntime::stop`] so accept unblocks from `accept().await`.
    accept_cancel_tx: Mutex<Option<flume::Sender<()>>>,
    registry: Arc<RwLock<HotRegistry>>,
    peers: RwLock<HashMap<u64, LivePeer>>,
    next_peer_id: AtomicU64,
    connected_out: RwLock<HashSet<(i64, SocketAddr)>>,
    /// Per-torrent cancel flags (bumped on stop so in-flight peers exit).
    torrent_cancel: RwLock<HashMap<i64, Arc<AtomicBool>>>,
    /// Serialize SQLite open/write so start/stop never sit behind a stuck open.
    catalog_mu: parking_lot::Mutex<()>,
    /// Long-lived catalog connection (opened under `catalog_mu`). Avoids remapping
    /// a multi‑hundred‑MB DB on every announce/start/list path.
    catalog: parking_lot::Mutex<Option<Catalog>>,
    /// Pending piece-haves → catalog bitfield (see [`PieceHaveBuffer`]).
    piece_have: PieceHaveBuffer,
    status: RwLock<String>,
    /// Session byte totals per torrent (for rates).
    torrent_bytes: RwLock<HashMap<i64, Arc<TorrentBytes>>>,
    rate_state: parking_lot::Mutex<HashMap<i64, RateSample>>,
    peer_rate_state: parking_lot::Mutex<HashMap<u64, RateSample>>,
    global_rate: parking_lot::Mutex<RateSample>,
    /// Dedicated piece-hash thread (CPU).
    hash: Arc<HashPool>,
    /// Dedicated disk thread (verified piece writes).
    #[allow(dead_code)] // kept alive for Drop/join; hash holds Arc to submit writes
    disk: Arc<DiskWorker>,
    /// Tracker announce schedule per torrent id.
    announce_sched: RwLock<HashMap<i64, TorrentAnnounce>>,
    /// Set after startup [`SessionRuntime::batch_announce_hot`] assigns
    /// `created_at DESC` stagger. Until then, activations use a far-future
    /// placeholder so tick cannot race-fire announces before the stagger lands.
    announce_stagger_applied: AtomicBool,
    /// Cap concurrent announces per tracker host (shared across torrents).
    host_limiter: Arc<HostLimiter>,
    /// rtorrent-style announce baselines, reset on each torrent **start**.
    /// `uploaded` / `downloaded` on the wire = current − baseline (not lifetime).
    announce_baseline: RwLock<HashMap<i64, AnnounceBaseline>>,
    /// Peer churn counters for periodic info summaries (not per-connection spam).
    peer_connects: AtomicU64,
    peer_disconnects: AtomicU64,
    /// Torrent ids with a detached recheck in flight (one per id).
    recheck_inflight: parking_lot::Mutex<HashSet<i64>>,
    /// Dial failure cooldown by tracker IP:listen-port.
    dial_cooldown: parking_lot::Mutex<HashMap<(i64, SocketAddr), DialData>>,
    /// Last tracker peer list per torrent (for min-peer refill between announces).
    last_tracker_peers: RwLock<HashMap<i64, Vec<SocketAddr>>>,
    /// Last tracker-reported swarm S/L per torrent (from successful announce).
    last_swarm: RwLock<HashMap<i64, (Option<u32>, Option<u32>)>>,
    /// Global wire rate limits (shared by all peer tasks). `0` cap = unlimited.
    wire_limiter: Arc<WireRateLimiter>,
    /// Live max peers per torrent (bootstrap from cfg; updated by apply_session_limits).
    max_peers: AtomicUsize,
    /// Live useful-peer floor (bootstrap from cfg; updated by apply_session_limits).
    min_peers: AtomicUsize,
}

impl Inner {
    /// Queue a verified piece-have for catalog durability (after RAM mark_have).
    fn queue_piece_have(&self, torrent_id: i64, index: u32, plen: u32) {
        self.piece_have
            .pending
            .lock()
            .push((torrent_id, index, plen));
    }
}

#[derive(Clone)]
pub struct SessionRuntime {
    inner: Arc<Inner>,
    pool: Arc<PeerWorkerPool>,
}

impl SessionRuntime {
    pub fn start(db: &Path, cfg: RuntimeConfig) -> Result<Self> {
        let workers = cfg.peer_workers.unwrap_or_else(default_peer_workers);
        let hash_n = cfg.hash_workers.unwrap_or_else(default_hash_workers);
        let pool = Arc::new(PeerWorkerPool::new(workers)?);
        // Design: separate hash workers and disk path (not on peer I/O threads).
        let disk = Arc::new(DiskWorker::spawn_with_options(
            cfg.discard_writes,
            &cfg.disk_backend,
            cfg.disk_depth,
        )?);
        // Hash pool shares DiskWorker; restart/status live on the same Arc.
        let hash = Arc::new(HashPool::spawn_n(disk.clone(), hash_n)?);
        let peer_id = generate_peer_id_with_prefix(&cfg.peer_id_prefix);
        let listen = cfg.listen;
        let host_limiter = Arc::new(HostLimiter::new(cfg.max_concurrent_per_host as usize));
        let wire_limiter = Arc::new(WireRateLimiter::new(
            cfg.max_upload_bps,
            cfg.max_download_bps,
        ));
        let max_peers = AtomicUsize::new(cfg.max_peers.max(1));
        let min_peers = AtomicUsize::new(cfg.min_peers.min(cfg.max_peers.max(1)).max(1));
        // Use catalog session limits when set.
        if let Ok(cat) = Catalog::open(db) {
            if let Ok(lim) = cat.session_limits() {
                let max_p = lim.max_peers.max(1) as usize;
                let min_p = (lim.min_peers as usize).min(max_p).max(1);
                max_peers.store(max_p, Ordering::Relaxed);
                min_peers.store(min_p, Ordering::Relaxed);
                wire_limiter.set_caps(lim.max_upload_bps, lim.max_download_bps);
            }
        }
        // Accept parks on Compio `TcpListener::accept`; stop drops sender to unblock.
        let (accept_cancel_tx, accept_cancel_rx) = flume::bounded::<()>(0);
        let inner = Arc::new(Inner {
            db: db.to_path_buf(),
            cfg,
            peer_id,
            stop: AtomicBool::new(false),
            accept_cancel_tx: Mutex::new(Some(accept_cancel_tx)),
            registry: Arc::new(RwLock::new(HotRegistry::new())),
            peers: RwLock::new(HashMap::new()),
            next_peer_id: AtomicU64::new(1),
            connected_out: RwLock::new(HashSet::new()),
            torrent_cancel: RwLock::new(HashMap::new()),
            catalog_mu: parking_lot::Mutex::new(()),
            catalog: parking_lot::Mutex::new(None),
            piece_have: PieceHaveBuffer {
                pending: Mutex::new(Vec::new()),
                // Far past so first complete/stop path can flush immediately.
                last_flush: Mutex::new(Instant::now() - PIECE_HAVE_FLUSH_INTERVAL),
            },
            status: RwLock::new(format!(
                "starting… io_workers={workers} (compio accept + least-peers)"
            )),
            torrent_bytes: RwLock::new(HashMap::new()),
            rate_state: parking_lot::Mutex::new(HashMap::new()),
            peer_rate_state: parking_lot::Mutex::new(HashMap::new()),
            global_rate: parking_lot::Mutex::new(RateSample::new()),
            hash: hash.clone(),
            disk: disk.clone(),
            announce_sched: RwLock::new(HashMap::new()),
            announce_stagger_applied: AtomicBool::new(false),
            host_limiter,
            announce_baseline: RwLock::new(HashMap::new()),
            peer_connects: AtomicU64::new(0),
            peer_disconnects: AtomicU64::new(0),
            recheck_inflight: parking_lot::Mutex::new(HashSet::new()),
            dial_cooldown: parking_lot::Mutex::new(HashMap::new()),
            last_tracker_peers: RwLock::new(HashMap::new()),
            last_swarm: RwLock::new(HashMap::new()),
            wire_limiter,
            max_peers,
            min_peers,
        });

        // Disk fatals → sticky TUI status (restart count / permanent death).
        let status_disk = Arc::clone(&inner);
        disk.set_status_hook(move |msg| {
            *status_disk.status.write() = msg;
        });

        let rt = Self {
            inner: inner.clone(),
            pool: pool.clone(),
        };

        // Accept on dedicated Compio accept thread; tick/bootstrap as service tasks.
        let pool_acc = pool.clone();
        let _ = pool.spawn_accept(move || {
            let inner = inner.clone();
            let pool = pool_acc;
            let accept_cancel_rx = accept_cancel_rx;
            async move {
                accept::accept_loop(inner, pool, accept_cancel_rx).await;
            }
        });
        let tick_rt = rt.clone();
        let _ = pool.spawn(move || {
            let tick_rt = tick_rt;
            async move {
                tick_loop(tick_rt).await;
            }
        });

        // Bootstrap want_start, then batch-announce all hot torrents (Compio on accept).
        let boot = rt.clone();
        let workers_n = workers;
        let _ = pool.spawn(move || {
            let boot = boot;
            async move {
                *boot.inner.status.write() = format!(
                    "listening {listen} · {} io workers (least-peers)",
                    boot.pool.workers()
                );
                // SQLite off accept event loop.
                let boot2 = boot.clone();
                let _ =
                    crate::runtime::PeerWorkerPool::run_blocking(move || boot2.sync_want_start())
                        .await;
                tracing::info!(
                    listen = %listen,
                    workers = workers_n,
                    "session ready"
                );
                boot.batch_announce_hot("startup").await;
            }
        });

        Ok(rt)
    }

    pub fn peer_workers(&self) -> usize {
        self.pool.workers()
    }

    pub fn hash_workers(&self) -> usize {
        self.inner.hash.workers()
    }

    /// Shared hash / recheck worker pool.
    pub fn hash_pool(&self) -> Arc<HashPool> {
        Arc::clone(&self.inner.hash)
    }

    /// True if this torrent is in the hot registry (started).
    pub fn is_hot(&self, id: i64) -> bool {
        self.inner.registry.read().get_id(id).is_some()
    }

    /// Claim exclusive detached recheck for `id`. Returns false if already running.
    pub fn try_begin_recheck(&self, id: i64) -> bool {
        self.inner.recheck_inflight.lock().insert(id)
    }

    /// Clear detached recheck claim (call when the recheck thread finishes).
    pub fn end_recheck(&self, id: i64) {
        self.inner.recheck_inflight.lock().remove(&id);
    }

    pub fn recheck_in_progress(&self, id: i64) -> bool {
        self.inner.recheck_inflight.lock().contains(&id)
    }

    pub fn stop(&self) {
        // Best-effort persist before peers tear down (covers process exit via shutdown()).
        self.flush_piece_haves(None, true);
        self.flush_uploaded_to_catalog();
        self.inner.stop.store(true, Ordering::SeqCst);
        // Wake accept loop (drop cancel sender → recv disconnects).
        drop(self.inner.accept_cancel_tx.lock().take());
        // Cancel all torrent peer tasks.
        for flag in self.inner.torrent_cancel.write().values() {
            flag.store(true, Ordering::SeqCst);
        }
        // Per-peer cancel + stop-wake (inbound has its own flag; outbound shares torrent cancel).
        for p in self.inner.peers.read().values() {
            p.signal_cancel();
        }
        *self.inner.status.write() = "stopped".into();
    }

    /// Signal stop, then shut down the peer I/O Compio pool from this thread.
    ///
    /// Before tearing down the pool, sends `event=stopped` for every hot torrent
    /// and **waits until each succeeds** (tracker accepted the announce). Retries
    /// on failure; quit does not proceed until all succeed. Explicit per-torrent
    /// stop remains fire-and-forget (see [`Self::stop_torrent`]).
    ///
    /// Must be called from a **non-async** context (control plane). Prefer
    /// [`PeerWorkerPool::shutdown`] over dropping the last pool `Arc` on a
    /// `seedchamp-io` worker.
    pub fn shutdown(self) {
        // Announce while hot state (UL/DL baselines, left) is still intact.
        self.announce_stopped_all();
        self.stop();
        // Give accept/tick/peer tasks a moment to observe the stop flag.
        std::thread::sleep(Duration::from_millis(100));
        self.pool.shutdown();
    }

    /// `event=stopped` for every active torrent, in parallel. Blocks until every
    /// torrent has a successful tracker response (retries with backoff).
    fn announce_stopped_all(&self) {
        if !self.inner.cfg.announce {
            return;
        }
        let jobs: Vec<_> = {
            let reg = self.inner.registry.read();
            reg.ids()
                .into_iter()
                .filter_map(|id| {
                    let t = reg.get_id(id)?;
                    let (uploaded, downloaded) = self.announce_transfer_totals(id);
                    Some((t, uploaded, downloaded))
                })
                .collect()
        };
        let n = jobs.len();
        if n == 0 {
            return;
        }

        let peer_id = self.inner.peer_id;
        let port = self.inner.cfg.listen.port();
        let limiter = self.inner.host_limiter.clone();
        let user_agent = self.inner.cfg.http_user_agent.clone();
        *self.inner.status.write() = format!("quitting — stopped announce 0/{n}…");
        tracing::info!(
            n,
            "quit: event=stopped for active torrents (wait for success)"
        );

        let inner = self.inner.clone();
        // Quit stopped-announce runs on the tracker Compio runtime (cyper HTTP).
        // Factory form: future is created on seedchamp-trk (!Send cyper/timers).
        self.pool.block_on_tracker(move || {
            let inner = inner;
            let jobs = jobs;
            let peer_id = peer_id;
            let port = port;
            let limiter = limiter;
            let user_agent = user_agent;
            let n = n;
            async move {
                use futures::stream::{FuturesUnordered, StreamExt};

                let mut pending = FuturesUnordered::new();
                for (t, uploaded, downloaded) in jobs {
                    let limiter = limiter.clone();
                    let user_agent = user_agent.clone();
                    let status_inner = inner.clone();
                    pending.push(async move {
                        let id = t.id;
                        let name = t.name.clone();
                        let mut attempt = 0u32;
                        loop {
                            attempt = attempt.saturating_add(1);
                            let out = announce_for(
                                &t,
                                &peer_id,
                                port,
                                Some("stopped"),
                                uploaded,
                                downloaded,
                                &limiter,
                                &user_agent,
                            )
                            .await;
                            if out.ok {
                                tracing::info!(
                                    id,
                                    torrent = %name,
                                    attempt,
                                    "quit: stopped announce ok"
                                );
                                return id;
                            }
                            let backoff_s = u64::from(attempt.min(10));
                            tracing::warn!(
                                id,
                                torrent = %name,
                                attempt,
                                backoff_s,
                                "quit: stopped announce failed — retrying"
                            );
                            // Surface retry in TUI status while quit UI is still painting.
                            *status_inner.status.write() = format!(
                                "quitting — #{id} stopped announce failed (try {attempt}) — retry {backoff_s}s"
                            );
                            compio::time::sleep(Duration::from_secs(backoff_s)).await;
                        }
                    });
                }

                let mut done = 0usize;
                while let Some(_id) = pending.next().await {
                    done += 1;
                    *inner.status.write() = format!("quitting — stopped announce {done}/{n} ok");
                }
                *inner.status.write() = format!("quitting — stopped announce {n}/{n} done");
                tracing::info!(n, "quit: all stopped announces succeeded");
            }
        });
    }

    pub fn is_running(&self) -> bool {
        !self.inner.stop.load(Ordering::SeqCst)
    }

    pub(crate) fn with_catalog_mut<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&mut Catalog) -> Result<T>,
    {
        let _guard = self.inner.catalog_mu.lock();
        let mut slot = self.inner.catalog.lock();
        if slot.is_none() {
            *slot = Some(Catalog::open(&self.inner.db)?);
        }
        f(slot.as_mut().unwrap())
    }

    /// Shared catalog read (control mutate worker / session paths). Holds `catalog_mu`.
    pub(crate) fn with_catalog<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Catalog) -> Result<T>,
    {
        let _guard = self.inner.catalog_mu.lock();
        let mut slot = self.inner.catalog.lock();
        if slot.is_none() {
            *slot = Some(Catalog::open(&self.inner.db)?);
        }
        f(slot.as_ref().unwrap())
    }

    /// Run catalog work on Compio's blocking pool so accept/peer/tracker tasks
    /// never park on `catalog_mu` / SQLite / slow disk under the mutex.
    ///
    /// Must be called from a Compio runtime task.
    async fn with_catalog_mut_async<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&mut Catalog) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let this = self.clone();
        crate::runtime::PeerWorkerPool::run_blocking(move || this.with_catalog_mut(f)).await?
    }

    async fn with_catalog_async<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Catalog) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let this = self.clone();
        crate::runtime::PeerWorkerPool::run_blocking(move || this.with_catalog(f)).await?
    }
}

async fn tick_loop(session: SessionRuntime) {
    let inner = session.inner.clone();
    let mut last_sync = Instant::now() - Duration::from_secs(10);
    let mut last_announce_poll = Instant::now() - Duration::from_secs(10);
    let mut last_peer_summary = Instant::now();
    // Resume interrupted leech_cache handoffs (home_root set + catalog complete).
    if let Ok(ids) = session.with_catalog(|cat| {
        let staged = cat.list_staged_leech_cache_ids()?;
        let mut out = Vec::new();
        for id in staged {
            if let Ok(Some(row)) = cat
                .list_torrents()
                .map(|rows| rows.into_iter().find(|r| r.id == id))
            {
                if row.complete {
                    out.push(id);
                }
            }
        }
        Ok(out)
    }) {
        for id in ids {
            session.maybe_start_leech_cache_handoff(id).await;
        }
    }
    while !inner.stop.load(Ordering::SeqCst) {
        // Bitfield durability: coalesce piece-haves; flush every 10s or on complete.
        // SQLite on blocking pool so seedchamp-io stays free for peer sockets.
        let became_complete = session.flush_piece_haves_async(None, false).await;
        for tid in became_complete {
            let name = session
                .inner
                .registry
                .read()
                .get_id(tid)
                .map(|t| t.name.clone())
                .unwrap_or_else(|| "?".into());
            *session.inner.status.write() = format!("#{tid} complete — seeding");
            tracing::info!(id = tid, torrent = %name, "complete — seeding");
            session.maybe_start_leech_cache_handoff(tid).await;
        }
        // RAM mark_have already ran; event=completed if fully complete.
        session.check_completed_announces();

        // Honor per-torrent tracker intervals (not a fixed 120s).
        if last_announce_poll.elapsed() >= Duration::from_secs(2) {
            last_announce_poll = Instant::now();
            session.poll_announce_due();
            // Catch completion that raced between piece ticks.
            session.check_completed_announces();
            // min_peers chase between announces (dial cache / last tracker list).
            //
            // **Must** snapshot ids into a Vec before the loop body:
            // 1) RwLockReadGuard is !Send across .await
            // 2) Rust extends for-header temporaries for the *entire* loop — so
            //    `for id in registry.read().ids() { refill… }` held registry.R
            //    while refill_peers waited on catalog_mu. start_torrent held
            //    catalog_mu and waited registry.W → permanent ABBA deadlock.
            let hot_ids = session.inner.registry.read().ids();
            for id in hot_ids {
                session.refill_peers(id, "tick").await;
                session.maybe_starve_announce(id);
            }
        }

        if last_sync.elapsed() >= Duration::from_secs(5) {
            last_sync = Instant::now();
            // Re-activate want_start torrents (SQLite + possible ensure_storage) off accept.
            let sess = session.clone();
            let _ =
                crate::runtime::PeerWorkerPool::run_blocking(move || sess.sync_want_start()).await;
            // New activations get schedule entries; poll next loop for announce.
            session.poll_announce_due();
        }

        // Peer churn summary (connect/disconnect counters), not per-socket spam.
        if last_peer_summary.elapsed() >= Duration::from_secs(60) {
            last_peer_summary = Instant::now();
            let up = inner.peer_connects.swap(0, Ordering::Relaxed);
            let down = inner.peer_disconnects.swap(0, Ordering::Relaxed);
            if up > 0 || down > 0 {
                let now = inner.peers.read().len();
                tracing::info!(
                    connected = up,
                    disconnected = down,
                    peers_now = now,
                    "peer activity (60s)"
                );
            }
            // Persist lifetime UP so catalog/SQL list stay current while seeding.
            let sess = session.clone();
            let _ = crate::runtime::PeerWorkerPool::run_blocking(move || {
                sess.flush_uploaded_to_catalog()
            })
            .await;
        }

        // Fixed 1s cadence on Compio accept runtime.
        compio::time::sleep(Duration::from_secs(1)).await;
    }
    // stop() already force-flushes; re-flush any races after the flag was set.
    let _ = session.flush_piece_haves_async(None, true).await;
}
