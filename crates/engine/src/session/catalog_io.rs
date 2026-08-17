//! Catalog durability helpers: piece-have flush, byte counters, announce baselines.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use super::announce::AnnounceBaseline;
use super::{TorrentBytes, PIECE_HAVE_FLUSH_INTERVAL};

impl super::SessionRuntime {
    pub(super) fn ensure_byte_counters(&self, id: i64) {
        let mut m = self.inner.torrent_bytes.write();
        m.entry(id).or_insert_with(|| {
            Arc::new(TorrentBytes {
                up: AtomicU64::new(0),
            })
        });
    }

    /// Initialize `torrent_bytes.up` from SQLite lifetime stats (if not already present).
    ///
    /// Without this, each start zeros RAM upload and the TUI used
    /// `catalog.max(session_delta)` — so session uploads never increased lifetime UP
    /// when the catalog already had any prior total.
    pub(super) fn seed_byte_counters_from_catalog(&self, id: i64) {
        let catalog_up = self.with_catalog(|cat| cat.stats_uploaded(id)).unwrap_or(0);
        let mut m = self.inner.torrent_bytes.write();
        m.entry(id)
            .and_modify(|b| {
                // Never lower a running counter (re-seed after race).
                let cur = b.up.load(Ordering::Relaxed);
                if catalog_up > cur {
                    b.up.store(catalog_up, Ordering::Relaxed);
                }
            })
            .or_insert_with(|| {
                Arc::new(TorrentBytes {
                    up: AtomicU64::new(catalog_up),
                })
            });
    }

    /// Persist RAM lifetime upload for all hot torrents (periodic + shutdown safety).
    pub(super) fn flush_uploaded_to_catalog(&self) {
        let snapshot: Vec<(i64, u64)> = self
            .inner
            .torrent_bytes
            .read()
            .iter()
            .map(|(id, b)| (*id, b.up.load(Ordering::Relaxed)))
            .filter(|(_, u)| *u > 0)
            .collect();
        if snapshot.is_empty() {
            return;
        }
        if let Err(e) = self.with_catalog_mut(|cat| cat.set_uploaded_batch(&snapshot)) {
            tracing::debug!(error = %e, "flush uploaded to catalog");
        }
    }

    /// Write pending piece-haves to SQLite bitfields.
    ///
    /// - `only_id = Some(id)`: flush that torrent only (stop_torrent).
    /// - `force = true`: ignore the 10s interval (stop / exit / complete path).
    /// - `force = false`: flush when interval elapsed **or** any pending torrent is
    ///   fully complete in RAM.
    ///
    /// Returns torrent ids that became complete in the catalog this flush.
    pub(super) fn flush_piece_haves(&self, only_id: Option<i64>, force: bool) -> Vec<i64> {
        // Lock order: never hold `pending` across registry/pieces (queue_piece_have
        // only takes pending; future code must not invent pending↔registry ABBA).
        let tids = {
            let pending = self.inner.piece_have.pending.lock();
            if pending.is_empty() {
                return Vec::new();
            }
            pending.iter().map(|(tid, _, _)| *tid).collect::<Vec<_>>()
        };
        let time_due =
            self.inner.piece_have.last_flush.lock().elapsed() >= PIECE_HAVE_FLUSH_INTERVAL;
        let any_complete = !force
            && tids.iter().any(|tid| {
                self.inner
                    .registry
                    .read()
                    .get_id(*tid)
                    .map(|t| t.is_complete())
                    .unwrap_or(false)
            });
        let due = force || any_complete || time_due;
        if !due {
            return Vec::new();
        }

        let batch = {
            let mut pending = self.inner.piece_have.pending.lock();
            if pending.is_empty() {
                return Vec::new();
            }
            match only_id {
                Some(id) => {
                    let mut take = Vec::new();
                    let mut keep = Vec::new();
                    for ev in pending.drain(..) {
                        if ev.0 == id {
                            take.push(ev);
                        } else {
                            keep.push(ev);
                        }
                    }
                    *pending = keep;
                    take
                }
                None => std::mem::take(&mut *pending),
            }
        };
        if batch.is_empty() {
            return Vec::new();
        }

        match self.with_catalog_mut(|cat| cat.mark_pieces_have_batch(&batch)) {
            Ok(became) => {
                *self.inner.piece_have.last_flush.lock() = Instant::now();
                became
            }
            Err(e) => {
                // Put events back so a later stop/exit can retry.
                self.inner.piece_have.pending.lock().extend(batch);
                tracing::warn!(error = %e, "mark_have batch");
                Vec::new()
            }
        }
    }

    pub(super) async fn flush_piece_haves_async(
        &self,
        only_id: Option<i64>,
        force: bool,
    ) -> Vec<i64> {
        // Avoid spawn_blocking when there is nothing to do.
        if !force && self.inner.piece_have.pending.lock().is_empty() {
            return Vec::new();
        }
        let this = self.clone();
        crate::runtime::PeerWorkerPool::run_blocking(move || this.flush_piece_haves(only_id, force))
            .await
            .unwrap_or_default()
    }

    /// Monotonic session upload counter for a torrent (source of truth for announce).
    ///
    /// Only `torrent_bytes.up` — must be incremented on **every** successful upload
    /// (`PeerConfig::on_upload`). Never recompute from live peers (that under-reports
    /// after disconnects and can confuse trackers).
    pub(super) fn raw_uploaded(&self, torrent_id: i64) -> u64 {
        self.inner
            .torrent_bytes
            .read()
            .get(&torrent_id)
            .map(|b| b.up.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// Snapshot baselines like libtorrent on torrent start:
    /// `uploaded_baseline = up_rate.total()`, `completed_baseline = completed_bytes()`.
    pub(super) fn reset_announce_baseline(&self, torrent_id: i64) {
        self.ensure_byte_counters(torrent_id);
        let uploaded = self.raw_uploaded(torrent_id);
        let completed = self
            .inner
            .registry
            .read()
            .get_id(torrent_id)
            .map(|t| t.completed_bytes())
            .unwrap_or(0);
        // Full-torrent incomplete only — priority-0 gaps must not count as "done"
        // for event=completed (private trackers / true complete).
        let incomplete_at_start = self
            .inner
            .registry
            .read()
            .get_id(torrent_id)
            .map(|t| !t.is_complete())
            .unwrap_or(false);
        self.inner.announce_baseline.write().insert(
            torrent_id,
            AnnounceBaseline {
                uploaded,
                completed,
                incomplete_at_start,
            },
        );
        tracing::debug!(
            id = torrent_id,
            uploaded_baseline = uploaded,
            completed_baseline = completed,
            incomplete_at_start,
            "announce baseline reset (rtorrent-style)"
        );
    }

    /// Send `event=completed` only when **every** piece is have (not merely wanted-done).
    /// Off / priority-0 missing pieces prevent completed.
    pub(super) fn maybe_request_completed_announce(&self, torrent_id: i64) {
        let incomplete_at_start = self
            .inner
            .announce_baseline
            .read()
            .get(&torrent_id)
            .map(|b| b.incomplete_at_start)
            .unwrap_or(false);
        if !incomplete_at_start {
            return;
        }
        let fully_complete = self
            .inner
            .registry
            .read()
            .get_id(torrent_id)
            .map(|t| t.is_complete())
            .unwrap_or(false);
        if !fully_complete {
            return;
        }
        let should_kick = {
            let mut sched = self.inner.announce_sched.write();
            let Some(entry) = sched.get_mut(&torrent_id) else {
                return;
            };
            if entry.sent_completed || entry.pending_completed {
                return;
            }
            entry.pending_completed = true;
            if !entry.in_flight {
                entry.next_due = Instant::now();
                true
            } else {
                false
            }
        };
        if should_kick {
            tracing::info!(
                id = torrent_id,
                "all pieces complete — announce event=completed"
            );
            self.kick_announce_now(torrent_id);
        }
    }

    /// Scan hot set for first-time download completion this start.
    pub(super) fn check_completed_announces(&self) {
        let ids: Vec<i64> = self.inner.registry.read().ids();
        for id in ids {
            self.maybe_request_completed_announce(id);
        }
    }

    /// Tracker `uploaded` / `downloaded` as rtorrent **adjusted** (since this start).
    ///
    /// Private-tracker critical:
    /// - uploaded = session_upload − baseline (monotonic within a start)
    /// - downloaded = completed_payload − baseline (**not** raw wire download)
    /// - first announce after start is typically 0/0 (or small)
    pub(super) fn announce_transfer_totals(&self, torrent_id: i64) -> (u64, u64) {
        let up_now = self.raw_uploaded(torrent_id);
        let completed_now = self
            .inner
            .registry
            .read()
            .get_id(torrent_id)
            .map(|t| t.completed_bytes())
            .unwrap_or(0);
        let base = self
            .inner
            .announce_baseline
            .read()
            .get(&torrent_id)
            .copied()
            .unwrap_or_default();
        (
            up_now.saturating_sub(base.uploaded),
            completed_now.saturating_sub(base.completed),
        )
    }
}
