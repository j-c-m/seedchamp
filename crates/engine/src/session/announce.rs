//! Tracker announce schedule and multi-tracker (BEP 12) announce policy.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::catalog::TrackerAnnounceUpdate;
use crate::error::{Error, Result};
use crate::hot::HotTorrent;
use crate::tracker::{announce_limited, AnnounceRequest, HostLimiter};

use super::dial::PEER_CACHE_KEEP;

/// Per-torrent tracker announce schedule (honors tracker `interval`).
pub(super) struct TorrentAnnounce {
    /// When the next announce may run.
    pub(super) next_due: Instant,
    /// Last interval seconds from tracker (clamped).
    pub(super) interval_secs: u32,
    /// Tracker `min interval` (clamped); starve re-requests must not go faster.
    pub(super) min_interval_secs: u32,
    /// Last successful announce time (for min-interval gates).
    pub(super) last_success: Option<Instant>,
    /// True after first successful `event=started` announce.
    pub(super) sent_started: bool,
    /// True after successful `event=completed` (once per start when we finish).
    pub(super) sent_completed: bool,
    /// Want `event=completed` on the next announce (rtorrent parity).
    pub(super) pending_completed: bool,
    /// In-flight announce (avoid double-fire).
    pub(super) in_flight: bool,
}

impl TorrentAnnounce {
    pub(super) fn fresh(now: Instant) -> Self {
        Self {
            next_due: now,
            interval_secs: ANNOUNCE_DEFAULT_SECS,
            min_interval_secs: ANNOUNCE_DEFAULT_MIN_INTERVAL_SECS,
            last_success: None,
            sent_started: false,
            sent_completed: false,
            pending_completed: false,
            in_flight: false,
        }
    }
}

pub(super) const ANNOUNCE_MIN_SECS: u32 = 60;
pub(super) const ANNOUNCE_DEFAULT_SECS: u32 = 1800;
pub(super) const ANNOUNCE_MAX_SECS: u32 = 6 * 3600;
/// Floor for tracker min interval (seconds).
pub(super) const ANNOUNCE_MIN_INTERVAL_FLOOR_SECS: u32 = 300;
/// Default when tracker omits min interval.
pub(super) const ANNOUNCE_DEFAULT_MIN_INTERVAL_SECS: u32 = 600;
pub(super) const ANNOUNCE_MIN_INTERVAL_CEIL_SECS: u32 = 4 * 3600;

pub(super) fn clamp_announce_interval(secs: u32) -> u32 {
    if secs == 0 {
        ANNOUNCE_DEFAULT_SECS
    } else {
        secs.clamp(ANNOUNCE_MIN_SECS, ANNOUNCE_MAX_SECS)
    }
}

/// Clamp tracker min interval like libtorrent TrackerState::set_min_interval.
pub(super) fn clamp_min_interval(secs: u32) -> u32 {
    if secs == 0 {
        ANNOUNCE_DEFAULT_MIN_INTERVAL_SECS
    } else {
        secs.clamp(
            ANNOUNCE_MIN_INTERVAL_FLOOR_SECS,
            ANNOUNCE_MIN_INTERVAL_CEIL_SECS,
        )
    }
}

/// Baselines for tracker announce (libtorrent `uploaded_baseline` / `completed_baseline`).
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct AnnounceBaseline {
    /// Cumulative upload counter at start.
    pub(super) uploaded: u64,
    /// `HotTorrent::completed_bytes()` at start.
    pub(super) completed: u64,
    /// True if **full** torrent was incomplete at start (`!is_complete()`).
    /// Off-file gaps keep this true until every piece is have.
    pub(super) incomplete_at_start: bool,
}

/// Result of a multi-tracker announce attempt (BEP 12 simplified).
#[derive(Debug, Clone)]
pub(super) struct AnnounceOutcome {
    pub peers: Vec<SocketAddr>,
    pub interval_secs: u32,
    pub min_interval_secs: u32,
    pub ok: bool,
    /// Tracker-reported seeders (`complete`).
    pub seeders: Option<u32>,
    /// Tracker-reported leechers (`incomplete`).
    pub leechers: Option<u32>,
    /// Failure/error status for the last tried URL (when `!ok`).
    pub last_status: Option<String>,
    /// Last URL tried (success or failure) — for catalog status updates.
    pub last_url: Option<String>,
}

impl AnnounceOutcome {
    fn empty_ok() -> Self {
        Self {
            peers: Vec::new(),
            interval_secs: ANNOUNCE_DEFAULT_SECS,
            min_interval_secs: ANNOUNCE_DEFAULT_MIN_INTERVAL_SECS,
            ok: true,
            seeders: None,
            leechers: None,
            last_status: None,
            last_url: None,
        }
    }

    fn all_failed(last_url: Option<String>, last_status: Option<String>) -> Self {
        Self {
            peers: Vec::new(),
            interval_secs: ANNOUNCE_DEFAULT_SECS,
            min_interval_secs: ANNOUNCE_DEFAULT_MIN_INTERVAL_SECS,
            ok: false,
            seeders: None,
            leechers: None,
            last_status,
            last_url,
        }
    }
}

/// **Multi-tracker (BEP 12 simplified, all events):**
/// - Tiers in order (0, then 1, …).
/// - Within a tier: try URLs **sequentially** (one at a time).
/// - On success: **stop** — do not try the rest of the tier or lower tiers.
/// - On failure: try the next URL in the same tier; only after the tier is
///   exhausted move to the next tier.
///
/// Avoids parallel races (multiple `event=completed` / started on the wire).
pub(super) async fn announce_for(
    t: &HotTorrent,
    peer_id: &[u8; 20],
    port: u16,
    event: Option<&'static str>,
    uploaded: u64,
    downloaded: u64,
    limiter: &Arc<HostLimiter>,
    user_agent: &str,
) -> AnnounceOutcome {
    // Trackers are cached on HotTorrent at activate — never reopen the catalog.
    let tiers = &t.tracker_tiers;
    if tiers.is_empty() {
        return AnnounceOutcome::empty_ok();
    }

    let t0 = Instant::now();
    let left_bytes = t.left_bytes();
    let name = t.name.clone();
    let infohash = t.infohash;
    // Dedupe the same URL if it appears more than once (any tier).
    let mut seen_urls = HashSet::new();
    let mut last_url: Option<String> = None;
    let mut last_status: Option<String> = None;

    for (tier, urls) in tiers {
        if urls.is_empty() {
            continue;
        }
        // Stable order within tier; skip empty / already-tried URLs.
        let mut tier_urls: Vec<&String> = Vec::with_capacity(urls.len());
        for url in urls {
            let key = url.trim().to_ascii_lowercase();
            if key.is_empty() || !seen_urls.insert(key) {
                continue;
            }
            tier_urls.push(url);
        }
        if tier_urls.is_empty() {
            continue;
        }

        for url in tier_urls {
            last_url = Some(url.clone());
            let req = AnnounceRequest {
                announce_url: url.clone(),
                infohash,
                peer_id: *peer_id,
                port,
                uploaded,
                downloaded,
                left: left_bytes,
                event,
                numwant: 80,
                user_agent: user_agent.to_string(),
                key: t.tracker_key,
            };
            match announce_limited(&req, limiter).await {
                Ok(r) if r.failure.is_none() => {
                    let iv = clamp_announce_interval(r.interval);
                    let min_iv = clamp_min_interval(r.min_interval);
                    tracing::info!(
                        torrent = %name,
                        tracker = %url,
                        tier,
                        peers = r.peers.len(),
                        seeders = ?r.complete,
                        leechers = ?r.incomplete,
                        interval = iv,
                        min_interval = min_iv,
                        event = ?event,
                        uploaded,
                        downloaded,
                        left = left_bytes,
                        elapsed_ms = t0.elapsed().as_millis() as u64,
                        "announce ok"
                    );
                    // First success wins — do not contact rest of tier or lower tiers.
                    return AnnounceOutcome {
                        peers: r.peers,
                        interval_secs: iv,
                        min_interval_secs: min_iv,
                        ok: true,
                        seeders: r.complete,
                        leechers: r.incomplete,
                        last_status: Some("ok".into()),
                        last_url: Some(url.clone()),
                    };
                }
                Ok(r) => {
                    let fail = r
                        .failure
                        .clone()
                        .unwrap_or_else(|| "tracker failure".into());
                    last_status = Some(fail.clone());
                    tracing::warn!(
                        torrent = %name,
                        tracker = %url,
                        tier,
                        failure = %fail,
                        event = ?event,
                        "announce fail — next in tier"
                    );
                }
                Err(e) => {
                    last_status = Some(e.to_string());
                    tracing::warn!(
                        torrent = %name,
                        tracker = %url,
                        tier,
                        error = %e,
                        event = ?event,
                        "announce err — next in tier"
                    );
                }
            }
        }
        tracing::debug!(torrent = %name, tier, "announce tier exhausted");
    }

    tracing::warn!(
        torrent = %t.name,
        event = ?event,
        elapsed_ms = t0.elapsed().as_millis() as u64,
        "announce: all trackers failed"
    );
    AnnounceOutcome::all_failed(last_url, last_status)
}

impl super::SessionRuntime {
    /// Announce due torrents (tracker interval) and connect peers for incomplete ones.
    pub(super) async fn announce_and_connect(&self, id: i64) -> Result<()> {
        let t = self
            .inner
            .registry
            .read()
            .get_id(id)
            .ok_or_else(|| Error::Msg(format!("torrent {id} not hot")))?;

        // Claim announce slot (may already be claimed by kick/poll before spawn).
        // Pick event (rtorrent order: started → completed → none).
        // For completed: optimistically mark sent so a concurrent/retry path cannot
        // fire a second event=completed while this one is in flight (private trackers).
        let event = {
            let mut sched = self.inner.announce_sched.write();
            let entry = sched
                .entry(id)
                .or_insert_with(|| TorrentAnnounce::fresh(Instant::now()));
            if !entry.in_flight {
                entry.in_flight = true;
            }
            if entry.pending_completed && entry.sent_started && !entry.sent_completed {
                entry.sent_completed = true;
                entry.pending_completed = false;
                Some("completed")
            } else if !entry.sent_started {
                Some("started")
            } else {
                None
            }
        };

        let manual = self.inner.cfg.manual_peers.clone();
        let mut interval = ANNOUNCE_DEFAULT_SECS;
        let mut any_ok = !self.inner.cfg.announce;
        if self.inner.cfg.announce {
            let info = t.clone();
            let peer_id = self.inner.peer_id;
            let port = self.inner.cfg.listen.port();
            let limiter = self.inner.host_limiter.clone();
            let user_agent = self.inner.cfg.http_user_agent.clone();
            let (uploaded, downloaded) = self.announce_transfer_totals(id);
            let out = announce_for(
                &info,
                &peer_id,
                port,
                event,
                uploaded,
                downloaded,
                &limiter,
                &user_agent,
            )
            .await;
            let tracker_peers = out.peers;
            interval = out.interval_secs;
            any_ok = out.ok;
            // RAM: last tracker compact list + live S/L for TUI.
            if !tracker_peers.is_empty() {
                self.inner
                    .last_tracker_peers
                    .write()
                    .insert(id, tracker_peers.clone());
            }
            if any_ok && (out.seeders.is_some() || out.leechers.is_some()) {
                self.inner
                    .last_swarm
                    .write()
                    .insert(id, (out.seeders, out.leechers));
            }
            // One catalog trip: upsert tracker → prune → upsert manuals → stats.
            // Manuals after prune so --peer addresses survive fat tracker lists.
            let tracker_update: Option<(String, TrackerAnnounceUpdate)> =
                out.last_url.as_ref().map(|url| {
                    let update = if any_ok {
                        TrackerAnnounceUpdate {
                            seeders: out.seeders,
                            leechers: out.leechers,
                            interval_secs: Some(out.interval_secs),
                            peers: Some(tracker_peers.len() as u32),
                            status: out.last_status.clone().unwrap_or_else(|| "ok".into()),
                            success: true,
                        }
                    } else {
                        TrackerAnnounceUpdate {
                            seeders: None,
                            leechers: None,
                            interval_secs: None,
                            peers: None,
                            status: out.last_status.clone().unwrap_or_else(|| "failed".into()),
                            success: false,
                        }
                    };
                    (url.clone(), update)
                });
            let need_catalog =
                !tracker_peers.is_empty() || !manual.is_empty() || tracker_update.is_some();
            if need_catalog {
                let manual_peers = manual.clone();
                if let Err(e) = self
                    .with_catalog_mut_async(move |cat| {
                        let tr = tracker_update.as_ref().map(|(url, u)| (url.as_str(), u));
                        cat.persist_after_announce(
                            id,
                            &tracker_peers,
                            &manual_peers,
                            PEER_CACHE_KEEP,
                            tr,
                        )
                    })
                    .await
                {
                    tracing::debug!(id, error = %e, "announce catalog persist failed");
                }
            }
            if any_ok {
                let mut sched = self.inner.announce_sched.write();
                if let Some(entry) = sched.get_mut(&id) {
                    entry.min_interval_secs = out.min_interval_secs;
                    entry.last_success = Some(Instant::now());
                }
            }
        } else if !manual.is_empty() {
            // Announce disabled: still remember manual peers for dial (no prune).
            let peers = manual.clone();
            let _ = self
                .with_catalog_mut_async(move |cat| {
                    cat.persist_after_announce(id, &[], &peers, PEER_CACHE_KEEP, None)
                })
                .await;
        }

        // Schedule next announce from tracker interval (retry sooner on total failure).
        let mut kick_completed_followup = false;
        {
            let mut sched = self.inner.announce_sched.write();
            if let Some(entry) = sched.get_mut(&id) {
                entry.in_flight = false;
                if any_ok {
                    match event {
                        Some("started") => entry.sent_started = true,
                        Some("completed") => {
                            // Already marked sent when the attempt started.
                            entry.sent_completed = true;
                            entry.pending_completed = false;
                        }
                        _ => {}
                    }
                    entry.interval_secs = interval;
                    // After started, if completed is pending, announce again immediately.
                    if entry.pending_completed && entry.sent_started && !entry.sent_completed {
                        entry.next_due = Instant::now();
                        kick_completed_followup = true;
                    } else {
                        entry.next_due = Instant::now() + Duration::from_secs(interval as u64);
                    }
                } else {
                    // Back off on total failure. Allow a later completed retry only
                    // if this attempt was the completed event and never landed.
                    if event == Some("completed") {
                        entry.sent_completed = false;
                        entry.pending_completed = true;
                    }
                    entry.next_due = Instant::now() + Duration::from_secs(300);
                }
            }
        }
        if kick_completed_followup {
            self.kick_announce_now(id);
        }

        // Dial toward max/min after announce (leech always; seed only if seed_dial_peers).
        self.refill_peers(id, "announce").await;

        if !t.is_download_complete() {
            *self.inner.status.write() = format!("#{id} leeching");
        } else if t.is_complete() {
            *self.inner.status.write() = format!("#{id} seeding");
        } else {
            *self.inner.status.write() = format!("#{id} seeding (wanted done)");
        }
        Ok(())
    }

    /// Schedule first announces for every hot torrent (startup / catch-up).
    ///
    /// Staggers `next_due` by `startup_stagger_ms` so we do not open all
    /// tracker connections at once. Slot order is **`created_at DESC`** (then
    /// `id DESC`) so newest torrents announce first. Actual work is driven by
    /// `poll_announce_due` and still gated by [`HostLimiter`] per tracker host.
    ///
    /// Interactive [`Self::start_torrent`] still uses [`Self::kick_announce_now`]
    /// (immediate; not reordered).
    pub(super) async fn batch_announce_hot(&self, reason: &str) {
        // Schedule even when tracker announce is off so incomplete torrents
        // still dial manual_peers (harness: --no-announce --peer …).
        let hot = self.inner.registry.read().ids();
        if hot.is_empty() {
            // Still mark applied so late activations use immediate schedule.
            self.inner
                .announce_stagger_applied
                .store(true, Ordering::Release);
            return;
        }
        // Newest first for initial stagger (catalog order, not HashMap walk).
        let ids = self
            .with_catalog(|cat| cat.order_ids_created_at_desc(&hot))
            .unwrap_or_else(|_| {
                let mut v = hot;
                v.sort_by(|a, b| b.cmp(a));
                v
            });
        let stagger = Duration::from_millis(self.inner.cfg.startup_stagger_ms);
        let n = ids.len();
        let now = Instant::now();
        {
            let mut sched = self.inner.announce_sched.write();
            for (i, id) in ids.into_iter().enumerate() {
                let due = if stagger.is_zero() {
                    now
                } else {
                    now + stagger.saturating_mul(i as u32)
                };
                sched
                    .entry(id)
                    .and_modify(|e| {
                        // Assign stagger slots. Must overwrite the far-future
                        // placeholders from sync_want_start (not only pull
                        // forward — that left every torrent due=now).
                        if !e.in_flight {
                            e.next_due = due;
                        }
                    })
                    .or_insert_with(|| TorrentAnnounce::fresh(due));
            }
        }
        self.inner
            .announce_stagger_applied
            .store(true, Ordering::Release);
        let spread_s = stagger.saturating_mul(n.saturating_sub(1) as u32).as_secs();
        *self.inner.status.write() = format!(
            "announce schedule ({reason}): {n} torrent(s), stagger={}ms, ~{spread_s}s spread, max_per_host={}",
            self.inner.cfg.startup_stagger_ms,
            self.inner.cfg.max_concurrent_per_host
        );
        // Kick due items immediately (first ones with zero delay).
        self.poll_announce_due();
    }

    /// Spawn announce/connect for one torrent now if not already in flight (user start).
    ///
    /// Claims `in_flight` under the schedule lock **before** spawn so kick + poll
    /// cannot race two concurrent announces for the same torrent.
    ///
    /// Always runs even when `announce=false`: [`Self::announce_and_connect`] still
    /// dials `manual_peers` (needed for `--no-announce --peer host:port` harnesses).
    pub(super) fn kick_announce_now(&self, id: i64) {
        if self.inner.registry.read().get_id(id).is_none() {
            return;
        }
        {
            let mut sched = self.inner.announce_sched.write();
            let entry = sched
                .entry(id)
                .or_insert_with(|| TorrentAnnounce::fresh(Instant::now()));
            if entry.in_flight {
                return;
            }
            entry.in_flight = true;
        }
        let this = self.clone();
        let _ = self.pool.spawn_tracker(move || {
            let this = this;
            async move {
                let _ = this.announce_and_connect(id).await;
            }
        });
    }

    /// Tick helper: announce (or re-dial manual peers) any torrent past `next_due`.
    ///
    /// Respects `max_inflight_announces` so we do not flood the runtime with
    /// concurrent announce tasks when many torrents share one host. Manual starts
    /// use [`Self::kick_announce_now`] instead so they are not delayed by the budget.
    ///
    /// When tracker announce is disabled, this still runs so incomplete torrents
    /// keep dialing `manual_peers` after disconnects.
    pub(super) fn poll_announce_due(&self) {
        let now = Instant::now();
        let max_inflight = self.inner.cfg.max_inflight_announces;
        // Snapshot hot ids first — never hold registry + announce_sched together
        // (opposite order vs start/stop → fair-RwLock risk under load).
        let hot_ids = self.inner.registry.read().ids();
        // Claim under write lock before spawn (same as kick_announce_now).
        let due: Vec<i64> = {
            let mut sched = self.inner.announce_sched.write();
            let inflight = sched.values().filter(|s| s.in_flight).count();
            let budget = if max_inflight == 0 {
                usize::MAX
            } else {
                (max_inflight as usize).saturating_sub(inflight)
            };
            if budget == 0 {
                return;
            }
            let mut ids: Vec<i64> = hot_ids
                .into_iter()
                .filter(|id| {
                    sched
                        .get(id)
                        .map(|s| !s.in_flight && now >= s.next_due)
                        .unwrap_or(true)
                })
                .collect();
            // Earliest due first (stagger order).
            ids.sort_by_key(|id| sched.get(id).map(|s| s.next_due).unwrap_or(now));
            ids.truncate(budget);
            let mut claimed = Vec::with_capacity(ids.len());
            for id in ids {
                let entry = sched
                    .entry(id)
                    .or_insert_with(|| TorrentAnnounce::fresh(now));
                if entry.in_flight {
                    continue;
                }
                entry.in_flight = true;
                claimed.push(id);
            }
            claimed
        };
        for id in due {
            let this = self.clone();
            let _ = self.pool.spawn_tracker(move || {
                let this = this;
                async move {
                    let _ = this.announce_and_connect(id).await;
                }
            });
        }
    }
}
