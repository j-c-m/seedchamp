//! Live peer / torrent snapshot types for TUI and control plane.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Instant;

use super::rates::{update_rate, RateSample};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerDirection {
    Inbound,
    Outbound,
}

/// Negotiated wire encryption for a live peer connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum PeerCrypto {
    /// Handshake not finished / not yet known.
    #[default]
    Unknown = 0,
    /// Classic BitTorrent (no MSE).
    Plain = 1,
    /// MSE/PE completed, selected plaintext.
    PePlain = 2,
    /// MSE/PE + RC4 stream.
    Rc4 = 3,
}

impl PeerCrypto {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => PeerCrypto::Plain,
            2 => PeerCrypto::PePlain,
            3 => PeerCrypto::Rc4,
            _ => PeerCrypto::Unknown,
        }
    }

    /// Short label for TUI columns.
    pub fn as_str(self) -> &'static str {
        match self {
            PeerCrypto::Unknown => "—",
            PeerCrypto::Plain => "plain",
            PeerCrypto::PePlain => "pe",
            PeerCrypto::Rc4 => "rc4",
        }
    }

    /// One-char crypto tag for combined dir+enc cells (`i-` / `ip` / `i4`).
    pub fn wire_tag(self) -> char {
        match self {
            PeerCrypto::Unknown => '?',
            PeerCrypto::Plain => '-',
            PeerCrypto::PePlain => 'p',
            PeerCrypto::Rc4 => '4',
        }
    }
}

/// Publish negotiated crypto to the live peer stats atom (TUI).
pub fn set_peer_crypto(slot: &Option<Arc<AtomicU8>>, crypto: PeerCrypto) {
    if let Some(a) = slot {
        a.store(crypto as u8, Ordering::Relaxed);
    }
}

#[derive(Debug, Clone)]
pub struct PeerInfo {
    pub id: u64,
    pub torrent_id: i64,
    pub torrent_name: String,
    pub addr: SocketAddr,
    pub direction: PeerDirection,
    pub uploaded: u64,
    pub downloaded: u64,
    pub upload_bps: u64,
    pub download_bps: u64,
    pub connected_secs: u64,
    /// Outstanding block requests on the wire (leecher).
    pub queue_outstanding: u64,
    /// Adaptive pipeline target for this peer.
    pub queue_target: u64,
    /// Peer sent Interested (wants data from us).
    pub peer_interested: bool,
    /// Remote is choking us (download Requests blocked unless Allowed Fast).
    pub peer_choking: bool,
    /// Local Interested flag for this peer.
    pub am_interested: bool,
    /// Upload requests currently being served to this peer.
    pub upload_pending: u64,
    /// Pieces the remote peer claims to have (from bitfield + HAVE).
    pub peer_have: u32,
    /// Torrent piece count (0 if not bound yet).
    pub piece_count: u32,
    /// Negotiated wire encryption.
    pub crypto: PeerCrypto,
    /// Remote client label from peer_id and/or LTEP `v` (empty while handshaking).
    pub client: String,
}

#[derive(Debug, Clone)]
pub struct TorrentLive {
    pub id: i64,
    pub name: String,
    pub complete: bool,
    pub have_count: u32,
    pub piece_count: u32,
    /// Full-torrent remaining (`total − completed`); tracker announce `left`.
    pub left: u64,
    pub peer_count: usize,
    /// Payload uploaded **this start** (`lifetime_uploaded − baseline`).
    pub session_uploaded: u64,
    /// Lifetime uploaded (catalog seed + this process). Monotonic while hot.
    pub lifetime_uploaded: u64,
    /// Verified payload downloaded **this start** (`completed − baseline`), rtorrent-style.
    /// Not raw wire; not lifetime. Use `completed_bytes` for absolute have totals.
    pub session_downloaded: u64,
    /// Absolute verified have payload (all have pieces). For lifetime DN display.
    pub completed_bytes: u64,
    /// Instantaneous upload rate (bytes/sec).
    pub upload_bps: u64,
    /// Instantaneous download rate (bytes/sec); from wire for smoothness.
    pub download_bps: u64,
    /// Seconds until next tracker announce (`None` if not scheduled / not hot).
    pub announce_in_secs: Option<u32>,
    /// Last tracker interval (seconds), if known.
    pub announce_interval_secs: Option<u32>,
    /// Announce request currently in flight.
    pub announce_in_flight: bool,
    /// Tracker-reported seeders from last successful announce (`None` if unknown).
    pub seeders: Option<u32>,
    /// Tracker-reported leechers from last successful announce.
    pub leechers: Option<u32>,
    /// Shared leech piece-buffer slots in use (`None` if no pool).
    pub staging_used: Option<u32>,
    /// Shared leech piece-buffer cap.
    pub staging_cap: Option<u32>,
    /// Staging RAM budget (bytes).
    pub staging_limit_bytes: Option<u64>,
    /// Exclusive piece claims (`in_flight`).
    pub staging_claims: u32,
}

#[derive(Debug, Clone, Default)]
pub struct SessionSnapshot {
    pub listen: String,
    pub peer_id_hex: String,
    pub running: bool,
    pub torrents: Vec<TorrentLive>,
    pub peers: Vec<PeerInfo>,
    pub status_line: String,
    pub total_upload_bps: u64,
    pub total_download_bps: u64,
    pub total_session_up: u64,
    pub total_session_down: u64,
    /// True when a nonblocking snapshot could not take a lock — UI should keep
    /// the previous frame (do not treat empty torrents/peers as “all stopped”).
    pub lock_busy: bool,
}

impl super::SessionRuntime {
    /// Full snapshot (may wait briefly on locks). Non-blocking: [`Self::snapshot_nonblocking`].
    pub fn snapshot(&self) -> SessionSnapshot {
        self.snapshot_inner(false)
    }

    /// TUI path: never block if registry/peers are briefly held; return last-known status.
    pub fn snapshot_nonblocking(&self) -> SessionSnapshot {
        self.snapshot_inner(true)
    }

    pub(super) fn snapshot_busy(&self) -> SessionSnapshot {
        SessionSnapshot {
            listen: self.inner.cfg.listen.to_string(),
            peer_id_hex: hex::encode(self.inner.peer_id),
            running: self.is_running(),
            status_line: self.status_line_for_snapshot(true),
            lock_busy: true,
            ..SessionSnapshot::default()
        }
    }

    /// Chatter status, overridden when the disk worker is permanently dead.
    pub(super) fn status_line_for_snapshot(&self, nonblocking: bool) -> String {
        if self.inner.disk.is_permanently_dead() {
            return crate::runtime::DISK_WORKER_DEAD_STATUS.to_string();
        }
        if nonblocking {
            self.inner
                .status
                .try_read()
                .map(|s| s.clone())
                .unwrap_or_default()
        } else {
            self.inner
                .status
                .try_read()
                .map(|s| s.clone())
                .unwrap_or_else(|| self.inner.status.read().clone())
        }
    }

    pub(super) fn snapshot_inner(&self, nonblocking: bool) -> SessionSnapshot {
        let reg = if nonblocking {
            match self.inner.registry.try_read() {
                Some(g) => g,
                None => return self.snapshot_busy(),
            }
        } else {
            self.inner.registry.read()
        };
        let peers_map = if nonblocking {
            match self.inner.peers.try_read() {
                Some(g) => g,
                None => return self.snapshot_busy(),
            }
        } else {
            self.inner.peers.read()
        };
        let bytes = if nonblocking {
            match self.inner.torrent_bytes.try_read() {
                Some(g) => g,
                None => return self.snapshot_busy(),
            }
        } else {
            self.inner.torrent_bytes.read()
        };
        let now = Instant::now();
        let sched = if nonblocking {
            self.inner.announce_sched.try_read()
        } else {
            Some(self.inner.announce_sched.read())
        };
        // One pass over peers → O(P + T), not O(T×P) per snapshot.
        let mut peer_stats: HashMap<i64, (usize, u64, u64)> = HashMap::new();
        for p in peers_map.values() {
            if p.torrent_id == 0 {
                continue;
            }
            let e = peer_stats.entry(p.torrent_id).or_insert((0, 0, 0));
            e.0 += 1;
            e.1 += p.up();
            e.2 += p.down();
        }
        // Never park on rate mutexes while holding registry/peers (fair waiters
        // then make every snapshot_nonblocking return lock_busy → frozen TUI).
        let mut rate_map = if nonblocking {
            match self.inner.rate_state.try_lock() {
                Some(g) => g,
                None => return self.snapshot_busy(),
            }
        } else {
            self.inner.rate_state.lock()
        };
        let mut total_up = 0u64;
        let mut total_down = 0u64;
        let mut total_wire_down = 0u64;
        let mut torrents = Vec::new();
        for id in reg.ids() {
            let Some(t) = reg.get_id(id) else { continue };
            let (peer_count, _peer_up, peer_down) =
                peer_stats.get(&id).copied().unwrap_or((0, 0, 0));
            let tb_up = bytes
                .get(&id)
                .map(|b| b.up.load(Ordering::Relaxed))
                .unwrap_or(0);
            // Lifetime upload = torrent_bytes only (seeded from catalog; +on every PIECE).
            // Never sum live peers — disconnects would drop the total.
            let lifetime_uploaded = tb_up;
            // Absolute have payload (verified pieces only — not raw wire).
            let completed_bytes = t.completed_bytes();
            // This-start baselines (rtorrent-style).
            let (base_up, base_completed) = if nonblocking {
                self.inner
                    .announce_baseline
                    .try_read()
                    .and_then(|m| m.get(&id).map(|b| (b.uploaded, b.completed)))
                    .unwrap_or((0, 0))
            } else {
                self.inner
                    .announce_baseline
                    .read()
                    .get(&id)
                    .map(|b| (b.uploaded, b.completed))
                    .unwrap_or((0, 0))
            };
            let session_uploaded = lifetime_uploaded.saturating_sub(base_up);
            let session_downloaded = completed_bytes.saturating_sub(base_completed);
            // Instantaneous ↓ rate still tracks wire (includes waste); smoother.
            let wire_down = peer_down;
            total_up += session_uploaded;
            total_down += session_downloaded;
            total_wire_down += wire_down;

            let (up_bps, down_bps) = {
                let sample = rate_map.entry(id).or_insert_with(RateSample::new);
                // Rate off lifetime (monotonic); peer-sum would glitch on disconnect.
                update_rate(sample, lifetime_uploaded, wire_down, now);
                (sample.up_bps, sample.down_bps)
            };

            let (announce_in_secs, announce_interval_secs, announce_in_flight) =
                match sched.as_ref().and_then(|m| m.get(&id)) {
                    Some(e) => {
                        let in_secs = e
                            .next_due
                            .saturating_duration_since(now)
                            .as_secs()
                            .min(u32::MAX as u64) as u32;
                        (Some(in_secs), Some(e.interval_secs), e.in_flight)
                    }
                    None => (None, None, false),
                };

            let (staging_used, staging_cap, staging_limit_bytes) = match t.staging_fill(nonblocking)
            {
                Some((u, c, lim)) => (Some(u as u32), Some(c as u32), Some(lim)),
                None => (None, None, None),
            };
            let staging_claims = t.in_flight_count(nonblocking) as u32;

            let (seeders, leechers) = if nonblocking {
                self.inner
                    .last_swarm
                    .try_read()
                    .and_then(|m| m.get(&id).copied())
                    .unwrap_or((None, None))
            } else {
                self.inner
                    .last_swarm
                    .read()
                    .get(&id)
                    .copied()
                    .unwrap_or((None, None))
            };

            torrents.push(TorrentLive {
                id,
                name: t.name.clone(),
                // "complete" for UI/rates = wanted files done (not full torrent).
                complete: t.is_download_complete(),
                have_count: t.have_count(),
                piece_count: t.piece_count,
                left: t.left_bytes(),
                peer_count,
                session_uploaded,
                lifetime_uploaded,
                session_downloaded,
                completed_bytes,
                upload_bps: up_bps,
                download_bps: down_bps,
                announce_in_secs,
                announce_interval_secs,
                announce_in_flight,
                seeders,
                leechers,
                staging_used,
                staging_cap,
                staging_limit_bytes,
                staging_claims,
            });
        }
        drop(rate_map);

        let (total_upload_bps, total_download_bps) = {
            let g = if nonblocking {
                self.inner.global_rate.try_lock()
            } else {
                Some(self.inner.global_rate.lock())
            };
            match g {
                Some(mut g) => {
                    update_rate(&mut g, total_up, total_wire_down, now);
                    (g.up_bps, g.down_bps)
                }
                None => (0, 0), // nonblocking miss — keep totals 0 this frame
            }
        };

        let mut peer_rates = if nonblocking {
            match self.inner.peer_rate_state.try_lock() {
                Some(g) => g,
                None => return self.snapshot_busy(),
            }
        } else {
            self.inner.peer_rate_state.lock()
        };
        let peers: Vec<PeerInfo> = peers_map
            .values()
            .map(|p| {
                let up = p.up();
                let down = p.down();
                let sample = peer_rates.entry(p.id).or_insert_with(RateSample::new);
                update_rate(sample, up, down, now);
                // client_label: try_lock so a peer mid-LTEP update cannot stall snapshot
                // under peers.read (which then freezes disconnect cleanup).
                let client = if nonblocking {
                    p.client_label
                        .try_lock()
                        .map(|g| g.clone())
                        .unwrap_or_default()
                } else {
                    p.client_label.lock().clone()
                };
                PeerInfo {
                    id: p.id,
                    torrent_id: p.torrent_id,
                    torrent_name: p.torrent_name.clone(),
                    addr: p.addr,
                    direction: p.direction,
                    uploaded: up,
                    downloaded: down,
                    upload_bps: sample.up_bps,
                    download_bps: sample.down_bps,
                    connected_secs: p.connected_at.elapsed().as_secs(),
                    queue_outstanding: p.queue_outstanding.load(Ordering::Relaxed),
                    queue_target: p.queue_target.load(Ordering::Relaxed),
                    peer_interested: p.peer_interested.load(Ordering::Relaxed),
                    peer_choking: p.peer_choking.load(Ordering::Relaxed),
                    am_interested: p.am_interested.load(Ordering::Relaxed),
                    upload_pending: p.upload_pending.load(Ordering::Relaxed),
                    peer_have: p.peer_have.load(Ordering::Relaxed),
                    piece_count: p.piece_count.load(Ordering::Relaxed),
                    crypto: PeerCrypto::from_u8(p.crypto.load(Ordering::Relaxed)),
                    client,
                }
            })
            .collect();
        let live_ids: HashSet<u64> = peers_map.keys().copied().collect();
        peer_rates.retain(|id, _| live_ids.contains(id));
        drop(peer_rates);
        // Drop registry/peers/bytes (and sched) before disk permanent-dead / status
        // so snapshot lock order is never registry → disk.state → status.
        drop(sched);
        drop(reg);
        drop(peers_map);
        drop(bytes);
        let status_line = self.status_line_for_snapshot(nonblocking);
        SessionSnapshot {
            listen: self.inner.cfg.listen.to_string(),
            peer_id_hex: hex::encode(self.inner.peer_id),
            running: self.is_running(),
            torrents,
            peers,
            status_line,
            total_upload_bps,
            total_download_bps,
            total_session_up: total_up,
            total_session_down: total_down,
            lock_busy: false,
        }
    }
}
