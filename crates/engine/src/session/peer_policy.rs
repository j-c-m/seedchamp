//! Peer slot limits, dial refill, and outbound spawn.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::hot::HotTorrent;
use crate::peer::{run_outbound_peer, PeerConfig};

use super::announce::ANNOUNCE_DEFAULT_MIN_INTERVAL_SECS;
use super::dial::{merge_outbound_peers, PEER_CACHE_DIAL_LOAD};
use super::dial_policy::{
    clear_dial_fail, is_cooled_down, light_disconnect_cooldown, record_dial_fail,
    record_dial_soft_fail, record_idle_close,
};
use super::snapshot::PeerCrypto;
use super::{LivePeer, PeerDirection, TorrentBytes};

impl super::SessionRuntime {
    /// Live peers bound to `torrent_id` (inbound + outbound). Unbound inbound (id 0) excluded.
    pub(super) fn peer_count_for_torrent(&self, torrent_id: i64) -> usize {
        self.inner
            .peers
            .read()
            .values()
            .filter(|p| p.torrent_id == torrent_id)
            .count()
    }

    /// Free peer slots for this torrent under live max_peers.
    pub(super) fn peer_slots_remaining(&self, torrent_id: i64) -> usize {
        let max = self.inner.max_peers.load(Ordering::Relaxed).max(1);
        max.saturating_sub(self.peer_count_for_torrent(torrent_id))
    }

    /// Useful peers counting toward [`RuntimeConfig::min_peers`].
    pub(super) fn useful_peer_count(&self, torrent_id: i64, we_need_download: bool) -> usize {
        let peers = self.inner.peers.read();
        peers
            .values()
            .filter(|p| p.torrent_id == torrent_id)
            .filter(|p| {
                let have = p.peer_have.load(Ordering::Relaxed);
                let pc = p.piece_count.load(Ordering::Relaxed);
                if pc == 0 || have == 0 {
                    return false;
                }
                if we_need_download {
                    // Can upload to us: has pieces and not choking.
                    !p.peer_choking.load(Ordering::Relaxed)
                } else {
                    // We can upload to them: incomplete + interested.
                    have < pc && p.peer_interested.load(Ordering::Relaxed)
                }
            })
            .count()
    }

    /// Whether outbound dial is allowed for this torrent's completeness state.
    pub(super) fn may_dial_out(&self, we_need_download: bool) -> bool {
        we_need_download || self.inner.cfg.seed_dial_peers
    }

    /// True if we already have a session keyed by tracker IP:listen-port.
    pub(super) fn already_have_peer(&self, torrent_id: i64, listen_addr: SocketAddr) -> bool {
        if self
            .inner
            .connected_out
            .read()
            .contains(&(torrent_id, listen_addr))
        {
            return true;
        }
        let peers = self.inner.peers.read();
        peers.values().any(|p| {
            if p.torrent_id != torrent_id {
                return false;
            }
            if p.addr == listen_addr {
                return true;
            }
            // Outbound records listen_port = dest port.
            if let Some(lp) = p.listen_port {
                p.addr.ip() == listen_addr.ip() && lp == listen_addr.port()
            } else {
                false
            }
        })
    }

    /// Immediate outbound dials for a torrent that needs data (no tracker wait).
    /// Sync path (mutation worker) — catalog SQLite is fine off the peer-io pool.
    pub(super) fn dial_leech_peers(&self, id: i64) {
        // Explicit reopen path always loads cache (skip early-out).
        let cached = self
            .with_catalog(|cat| cat.list_peer_cache(id, PEER_CACHE_DIAL_LOAD))
            .unwrap_or_else(|_| Vec::new());
        self.refill_peers_with_cache(id, "file-on / reopen", cached);
    }

    /// Shared dial-policy gate for P0 SQLite skip and actual refill.
    ///
    /// Returns `(hot, min_peers, useful, slots)` when this torrent should chase
    /// more outbound peers: dial allowed, free slots, and useful &lt; min.
    /// Keep this the single source of truth if dial policy evolves (e.g. chase
    /// max slots beyond min useful).
    pub(super) fn peer_refill_plan(
        &self,
        id: i64,
    ) -> Option<(Arc<HotTorrent>, usize, usize, usize)> {
        let t = self.inner.registry.read().get_id(id)?;
        let we_need = !t.is_download_complete();
        if !self.may_dial_out(we_need) {
            return None;
        }
        let slots = self.peer_slots_remaining(id);
        if slots == 0 {
            return None;
        }
        // Chase free max slots while useful < min (many peers stay choked).
        let max_p = self.inner.max_peers.load(Ordering::Relaxed).max(1);
        let min_peers = self
            .inner
            .min_peers
            .load(Ordering::Relaxed)
            .min(max_p)
            .max(1);
        let useful = self.useful_peer_count(id, we_need);
        if useful >= min_peers {
            return None;
        }
        Some((t, min_peers, useful, slots))
    }

    /// True when this torrent may need more outbound dials (RAM checks only).
    ///
    /// Used to skip `list_peer_cache` on the 2s tick for seeds already at min_peers.
    pub(super) fn needs_peer_refill(&self, id: i64) -> bool {
        self.peer_refill_plan(id).is_some()
    }

    /// Async dial path: peer-cache SQLite on the blocking pool (never parks seedchamp-io).
    pub(super) async fn refill_peers(&self, id: i64, reason: &str) {
        // P0: do not open SQLite when already at min_peers / no slots / seed-dial off.
        if !self.needs_peer_refill(id) {
            return;
        }
        let cached = self
            .with_catalog_async(move |cat| cat.list_peer_cache(id, PEER_CACHE_DIAL_LOAD))
            .await
            .unwrap_or_else(|_| Vec::new());
        self.refill_peers_with_cache(id, reason, cached);
    }

    /// Dial toward min/max from cache + last tracker + manual (cooldown-aware).
    pub(super) fn refill_peers_with_cache(&self, id: i64, reason: &str, cached: Vec<SocketAddr>) {
        // Same gates as needs_peer_refill (single peer_refill_plan).
        let Some((t, min_peers, useful, slots)) = self.peer_refill_plan(id) else {
            return;
        };
        let manual = self.inner.cfg.manual_peers.clone();
        let tracker = self
            .inner
            .last_tracker_peers
            .read()
            .get(&id)
            .cloned()
            .unwrap_or_default();
        if manual.is_empty() && cached.is_empty() && tracker.is_empty() {
            return;
        }
        let tracker_n = tracker.len();
        let cache_n = cached.len();
        let mut dial = merge_outbound_peers(&manual, tracker, cached, slots);
        let now = Instant::now();
        dial.retain(|addr| {
            if self.already_have_peer(id, *addr) {
                return false;
            }
            let cool = self.inner.dial_cooldown.lock();
            !is_cooled_down(&cool, id, *addr, now)
        });
        if dial.is_empty() {
            return;
        }
        tracing::info!(
            id,
            torrent = %t.name,
            n = dial.len(),
            slots,
            useful,
            min_peers,
            tracker = tracker_n,
            cache = cache_n,
            reason,
            "dialing peers"
        );
        for addr in dial {
            self.spawn_outbound(t.clone(), addr);
        }
    }

    /// If under min_peers and dial pool empty, pull announce forward subject to min interval.
    pub(super) fn maybe_starve_announce(&self, id: i64) {
        let Some(t) = self.inner.registry.read().get_id(id) else {
            return;
        };
        let we_need = !t.is_download_complete();
        if !self.may_dial_out(we_need) {
            return;
        }
        let max_p = self.inner.max_peers.load(Ordering::Relaxed).max(1);
        let min_peers = self
            .inner
            .min_peers
            .load(Ordering::Relaxed)
            .min(max_p)
            .max(1);
        if self.useful_peer_count(id, we_need) >= min_peers {
            return;
        }
        if self.peer_slots_remaining(id) == 0 {
            return;
        }
        let now = Instant::now();
        let mut sched = self.inner.announce_sched.write();
        let Some(entry) = sched.get_mut(&id) else {
            return;
        };
        if entry.in_flight {
            return;
        }
        let min_iv = entry
            .min_interval_secs
            .max(ANNOUNCE_DEFAULT_MIN_INTERVAL_SECS);
        if let Some(last) = entry.last_success {
            if now < last + Duration::from_secs(min_iv as u64) {
                return;
            }
        }
        // Pull next_due forward if later than now.
        if entry.next_due > now {
            entry.next_due = now;
            tracing::debug!(id, min_iv, "starve: pull announce due (under min_peers)");
        }
    }

    pub(super) fn spawn_outbound(&self, torrent: Arc<HotTorrent>, addr: SocketAddr) {
        let key = (torrent.id, addr);
        if self.already_have_peer(torrent.id, addr) {
            return;
        }
        {
            let cool = self.inner.dial_cooldown.lock();
            if is_cooled_down(&cool, torrent.id, addr, Instant::now()) {
                return;
            }
        }
        if !self.inner.connected_out.write().insert(key) {
            return;
        }
        let cancel = self
            .inner
            .torrent_cancel
            .read()
            .get(&torrent.id)
            .cloned()
            .unwrap_or_else(|| Arc::new(AtomicBool::new(true))); // no flag → already stopped
        if cancel.load(Ordering::SeqCst) {
            self.inner.connected_out.write().remove(&key);
            return;
        }

        // Reserve a peer slot synchronously so concurrent dials honor max_peers.
        let peer_down = Arc::new(AtomicU64::new(0));
        let peer_up = Arc::new(AtomicU64::new(0));
        let q_out = Arc::new(AtomicU64::new(0));
        // 0 until leech pipe is active — non-zero target makes TUI show "0/32"
        // and hides seeder QUEUE (int / int:N) even while uploading.
        let q_tgt = Arc::new(AtomicU64::new(0));
        let interested = Arc::new(AtomicBool::new(false));
        // Start choking until peer Unchoke (TUI diagnosis).
        let peer_choking = Arc::new(AtomicBool::new(true));
        let am_interested = Arc::new(AtomicBool::new(true)); // outbound leech always starts Interested
        let up_pend = Arc::new(AtomicU64::new(0));
        let peer_have = Arc::new(AtomicU32::new(0));
        let piece_count = Arc::new(AtomicU32::new(torrent.piece_count));
        let crypto = Arc::new(AtomicU8::new(PeerCrypto::Unknown as u8));
        let client_label = Arc::new(Mutex::new(String::new()));
        let tid = torrent.id;
        let tname = torrent.name.clone();
        let stop_flag = cancel.clone();
        let (pid, stop_rx) = {
            let max = self.inner.max_peers.load(Ordering::Relaxed).max(1);
            let mut peers = self.inner.peers.write();
            let n = peers.values().filter(|p| p.torrent_id == tid).count();
            if n >= max {
                drop(peers);
                self.inner.connected_out.write().remove(&key);
                return;
            }
            let pid = self.inner.next_peer_id.fetch_add(1, Ordering::Relaxed);
            let (stop_tx, stop_rx) = flume::bounded::<()>(0);
            peers.insert(
                pid,
                LivePeer {
                    id: pid,
                    torrent_id: tid,
                    torrent_name: tname,
                    addr,
                    direction: PeerDirection::Outbound,
                    wire_up: Some(peer_up.clone()),
                    wire_down: Some(peer_down.clone()),
                    connected_at: Instant::now(),
                    cancel: cancel.clone(),
                    stop_tx: Mutex::new(Some(stop_tx)),
                    queue_outstanding: q_out.clone(),
                    queue_target: q_tgt.clone(),
                    peer_interested: interested.clone(),
                    peer_choking: peer_choking.clone(),
                    am_interested: am_interested.clone(),
                    upload_pending: up_pend.clone(),
                    peer_have: peer_have.clone(),
                    piece_count: piece_count.clone(),
                    crypto: crypto.clone(),
                    client_label: client_label.clone(),
                    listen_port: Some(addr.port()),
                },
            );
            (pid, stop_rx)
        };

        let inner = self.inner.clone();
        let peer_id = self.inner.peer_id;
        let cfg = self.inner.cfg.clone();
        let hash = self.inner.hash.clone();
        let connected_at = Instant::now();
        let idle_closed = Arc::new(AtomicBool::new(false));
        let on_piece = {
            let inner = self.inner.clone();
            Arc::new(move |tid, idx, len| {
                inner.queue_piece_have(tid, idx, len);
            }) as Arc<dyn Fn(i64, u32, u32) + Send + Sync>
        };

        let _ = self.pool.spawn_peer(move || {
            let inner = inner;
            let cancel = cancel;
            let peer_down = peer_down;
            let peer_up = peer_up;
            let q_out = q_out;
            let q_tgt = q_tgt;
            let interested = interested;
            let peer_choking = peer_choking;
            let am_interested = am_interested;
            let up_pend = up_pend;
            let peer_have = peer_have;
            let piece_count = piece_count;
            let crypto = crypto;
            let client_label = client_label;
            let hash = hash;
            let on_piece = on_piece;
            let stop_flag = stop_flag;
            let stop_rx = stop_rx;
            let torrent = torrent;
            let cfg = cfg;
            async move {
                if inner.stop.load(Ordering::SeqCst) || cancel.load(Ordering::SeqCst) {
                    inner.peers.write().remove(&pid);
                    inner.peer_rate_state.lock().remove(&pid);
                    inner.connected_out.write().remove(&(tid, addr));
                    return;
                }

                // Cancel drops LivePeer.stop_tx so duplex parks wake (no poller).

                // Every uploaded byte must hit torrent_bytes immediately (private trackers).
                let on_upload = {
                    let i = inner.clone();
                    Arc::new(move |_tid: i64, n: u64| {
                        let mut map = i.torrent_bytes.write();
                        let e = map.entry(tid).or_insert_with(|| {
                            Arc::new(TorrentBytes {
                                up: AtomicU64::new(0),
                            })
                        });
                        e.up.fetch_add(n, Ordering::Relaxed);
                    }) as Arc<dyn Fn(i64, u64) + Send + Sync>
                };
                let pcfg = PeerConfig {
                    peer_id,
                    encryption: cfg.encryption,
                    pipeline: cfg.pipeline,
                    pipeline_max: cfg.pipeline_max,
                    staging_mem_limit: cfg.staging_mem_limit,
                    upload: cfg.upload,
                    // Always seed-while-leech unless discard_writes (no durable payload to serve).
                    allow_upload: !cfg.discard_writes,
                    allow_download: true,
                    wire_down: Some(peer_down),
                    wire_up: Some(peer_up),
                    on_upload: Some(on_upload),
                    queue_outstanding: Some(q_out),
                    queue_target: Some(q_tgt),
                    peer_interested: Some(interested),
                    peer_choking: Some(peer_choking),
                    am_interested: Some(am_interested),
                    upload_pending: Some(up_pend),
                    peer_have: Some(peer_have),
                    piece_count: Some(piece_count),
                    crypto: Some(crypto),
                    client_label: Some(client_label),
                    ltep_client: cfg.ltep_client.clone(),
                    listen_port: cfg.listen.port(),
                    hash: Some(hash),
                    on_piece: Some(on_piece),
                    stop: Some(stop_flag),
                    stop_rx: Some(stop_rx),
                    on_bound: None,
                    redundant_seed_idle: Duration::from_secs(cfg.redundant_seed_idle_secs),
                    useless_peer_idle: Duration::from_secs(cfg.useless_peer_idle_secs),
                    send_buffer_bytes: cfg.send_buffer_bytes,
                    recv_buffer_bytes: cfg.recv_buffer_bytes,
                    wire_limiter: Some(inner.wire_limiter.clone()),
                    idle_closed: Some(idle_closed.clone()),
                };
                inner.peer_connects.fetch_add(1, Ordering::Relaxed);
                let result = run_outbound_peer(addr, torrent, pcfg).await;
                let now = Instant::now();
                let lived = now.saturating_duration_since(connected_at);
                {
                    let mut cool = inner.dial_cooldown.lock();
                    if idle_closed.load(Ordering::Relaxed) {
                        record_idle_close(&mut cool, tid, addr, now);
                    } else {
                        match &result {
                            Err(_) => record_dial_fail(&mut cool, tid, addr, now),
                            Ok(()) if lived < Duration::from_secs(5) => {
                                record_dial_soft_fail(&mut cool, tid, addr, now);
                            }
                            Ok(()) if lived >= Duration::from_secs(30) => {
                                clear_dial_fail(&mut cool, tid, addr);
                            }
                            Ok(()) => light_disconnect_cooldown(&mut cool, tid, addr, now),
                        }
                    }
                }
                if let Err(e) = result {
                    tracing::debug!(%addr, error = %e, "outbound end");
                }
                inner.peer_disconnects.fetch_add(1, Ordering::Relaxed);
                inner.peers.write().remove(&pid);
                inner.peer_rate_state.lock().remove(&pid);
                inner.connected_out.write().remove(&(tid, addr));
            }
        });
    }
}
