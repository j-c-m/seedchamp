//! Torrent lifecycle: start/stop, want_start sync, delete/remove, file priority.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::disk::ensure_storage;
use crate::error::{Error, Result};
use crate::hot::HotRegistry;

use super::announce::{announce_for, TorrentAnnounce};

impl super::SessionRuntime {
    /// Catalog + activate (may touch disk). Announce/connect is always background.
    /// Runs on the serial mutation worker — never on the TUI thread.
    ///
    /// **Locking:** SQLite under `catalog_mu` only. `ensure_storage` (create/set_len)
    /// runs **outside** the catalog mutex so a large multi-file torrent cannot pin
    /// every peer-io worker behind `catalog_mu` for minutes.
    pub fn start_torrent(&self, id: i64) -> Result<()> {
        // Fresh cancel flag for this run (invalidates any peers from a prior stop).
        {
            let mut m = self.inner.torrent_cancel.write();
            if let Some(old) = m.get(&id) {
                old.store(true, Ordering::SeqCst);
            }
            m.insert(id, Arc::new(AtomicBool::new(false)));
        }

        if self.inner.registry.read().get_id(id).is_some() {
            // Already hot — only flip want_start / re-kick announce.
            self.with_catalog_mut(|cat| cat.set_want_start(id, true))?;
        } else {
            // 1) Load under catalog lock only (no disk prep).
            let hot = self.with_catalog_mut(|cat| {
                cat.set_want_start(id, true)?;
                HotRegistry::load_from_catalog(cat, id, true)
            })?;
            // 2) Disk prep without catalog_mu (can be slow on multi-file torrents).
            if !self.inner.cfg.discard_writes {
                ensure_storage(&hot.layout())?;
            }
            hot.set_staging_mem_limit(self.inner.cfg.staging_mem_limit);
            if !hot.is_download_complete() {
                hot.ensure_staging_pool();
            }
            // 3) Publish hot (race-safe if another start won).
            let mut reg = self.inner.registry.write();
            if reg.get_id(id).is_none() {
                reg.insert(hot);
            }
        }

        // Seed lifetime counters from catalog so UP is not reset each start.
        self.seed_byte_counters_from_catalog(id);
        // rtorrent: reset uploaded/completed baselines on each start so trackers
        // see per-run deltas, not lifetime totals.
        self.reset_announce_baseline(id);
        // User start: fire announce immediately (not wait for the 2s poll tick or
        // global inflight batch). Per-host limiter still applies inside announce.
        {
            let mut sched = self.inner.announce_sched.write();
            sched.insert(id, TorrentAnnounce::fresh(Instant::now()));
        }
        *self.inner.status.write() = format!("#{id} starting — announcing…");
        self.kick_announce_now(id);
        Ok(())
    }

    /// Persist file on/off (`priority` 0 = off, ≥1 = on) and update hot state if running.
    pub fn set_file_priority(&self, torrent_id: i64, file_idx: u32, priority: i32) -> Result<()> {
        self.with_catalog_mut(|cat| cat.set_file_priority(torrent_id, file_idx, priority))?;
        let need_leech = if let Some(hot) = self.inner.registry.read().get_id(torrent_id) {
            hot.set_file_priority(file_idx, priority);
            // layout.files[].priority is load-time; use live prios so a newly-on
            // file is created/set_len without restart.
            if priority > 0 && !self.inner.cfg.discard_writes {
                let prios = hot.file_priority.read().clone();
                if let Err(e) =
                    crate::disk::ensure_storage_with_priorities(&hot.layout(), Some(&prios))
                {
                    tracing::warn!(
                        torrent_id,
                        file_idx,
                        error = %e,
                        "ensure_storage after file on"
                    );
                }
            }
            !hot.is_download_complete()
        } else {
            false
        };
        // Turning files back on reopens leech from peer_cache + manual only.
        // Do not kick_announce_now: that bypasses tracker interval / min_interval.
        // Connected peers re-send Interested when they see missing work again.
        if need_leech {
            self.dial_leech_peers(torrent_id);
        }
        Ok(())
    }

    /// Soft-delete: `mark_deleted` via the session catalog.
    ///
    /// Serial mutation worker only. Requires the torrent to be **stopped** (not
    /// hot; catalog `want_start=0`). Rejects if a detached recheck is in flight
    /// (no wait — deadlock-free). Does not open a second SQLite connection.
    pub fn soft_delete_torrent(&self, id: i64) -> Result<()> {
        if self.recheck_in_progress(id) {
            return Err(Error::Msg(format!(
                "torrent #{id} recheck in progress; wait or cancel before delete"
            )));
        }
        if self.is_hot(id) {
            return Err(Error::Msg(format!("torrent #{id} is started")));
        }
        // Catalog also rejects want_start != 0 (race / cold want_start).
        self.with_catalog_mut(|cat| cat.mark_deleted(id))
    }

    /// Hard-remove catalog rows (CASCADE); payload on disk is left alone.
    ///
    /// Serial mutation worker only. Same stopped requirement as soft-delete.
    pub fn remove_torrent_catalog(&self, id: i64) -> Result<()> {
        if self.recheck_in_progress(id) {
            return Err(Error::Msg(format!(
                "torrent #{id} recheck in progress; wait or cancel before remove"
            )));
        }
        if self.is_hot(id) {
            return Err(Error::Msg(format!("torrent #{id} is started")));
        }
        self.with_catalog_mut(|cat| cat.remove_torrent(id))
    }

    pub fn stop_torrent(&self, id: i64) -> Result<()> {
        // Checkpoint bitfield before dropping hot state (throttled path may lag ≤10s).
        self.flush_piece_haves(Some(id), true);
        // Best-effort stopped announce while torrent is still hot.
        if self.inner.cfg.announce {
            if let Some(t) = self.inner.registry.read().get_id(id) {
                let peer_id = self.inner.peer_id;
                let port = self.inner.cfg.listen.port();
                let limiter = self.inner.host_limiter.clone();
                let user_agent = self.inner.cfg.http_user_agent.clone();
                let (uploaded, downloaded) = self.announce_transfer_totals(id);
                let t = t.clone();
                let _ = self.pool.spawn_tracker(move || {
                    let t = t;
                    async move {
                        let _ = announce_for(
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
                    }
                });
            }
        }

        // Cancel peers + drop hot set first (in-memory) so stop always "takes".
        if let Some(flag) = self.inner.torrent_cancel.write().remove(&id) {
            flag.store(true, Ordering::SeqCst);
        }
        let infohash = self.inner.registry.read().get_id(id).map(|t| t.infohash);
        if let Some(ih) = infohash {
            self.inner.registry.write().remove(&ih);
        }
        self.inner
            .connected_out
            .write()
            .retain(|(tid, _)| *tid != id);
        {
            let mut peers = self.inner.peers.write();
            let mut rates = self.inner.peer_rate_state.lock();
            peers.retain(|pid, p| {
                if p.torrent_id == id {
                    p.signal_cancel();
                    rates.remove(pid);
                    false
                } else {
                    true
                }
            });
        }
        // Flush lifetime upload into SQLite before dropping RAM counters.
        let up = self.raw_uploaded(id);
        self.inner.announce_sched.write().remove(&id);
        self.inner.announce_baseline.write().remove(&id);
        self.inner.torrent_bytes.write().remove(&id);
        self.inner.rate_state.lock().remove(&id);
        self.inner.last_tracker_peers.write().remove(&id);
        self.inner.last_swarm.write().remove(&id);
        *self.inner.status.write() = format!("stopped #{id}");

        // Persist want_start=0 + uploaded under catalog mutex.
        self.with_catalog_mut(|cat| {
            if up > 0 {
                cat.set_uploaded_at_least(id, up)?;
            }
            cat.set_want_start(id, false)?;
            Ok(())
        })?;
        Ok(())
    }

    pub fn sync_want_start(&self) -> Result<()> {
        if !self.inner.db.exists() {
            return Ok(());
        }
        // Keep state column consistent if someone flipped want_start in SQLite only.
        let _ = self.with_catalog_mut(|cat| cat.sync_state_with_want_start());
        // Only rows that want to run — do not full-scan the catalog every tick
        // (700 stopped torrents must stay cheap).
        let ids = self.with_catalog(|cat| cat.list_want_start_ids())?;
        for id in ids {
            if self.inner.registry.read().get_id(id).is_some() {
                continue; // already hot — don't re-announce every tick
            }
            {
                let mut m = self.inner.torrent_cancel.write();
                m.entry(id)
                    .or_insert_with(|| Arc::new(AtomicBool::new(false)));
            }
            // Load under catalog only — never ensure_storage while holding catalog_mu.
            let hot = match self.with_catalog_mut(|cat| {
                cat.set_want_start(id, true)?;
                HotRegistry::load_from_catalog(cat, id, true)
            }) {
                Ok(h) => h,
                Err(e) => {
                    tracing::warn!(id, error = %e, "activate failed");
                    continue;
                }
            };
            if !self.inner.cfg.discard_writes {
                if let Err(e) = ensure_storage(&hot.layout()) {
                    tracing::warn!(id, error = %e, "activate ensure_storage failed");
                    continue;
                }
            }
            hot.set_staging_mem_limit(self.inner.cfg.staging_mem_limit);
            if !hot.is_download_complete() {
                hot.ensure_staging_pool();
            }
            {
                let mut reg = self.inner.registry.write();
                if reg.get_id(id).is_none() {
                    reg.insert(hot);
                }
            }
            self.seed_byte_counters_from_catalog(id);
            self.reset_announce_baseline(id);
            {
                let mut sched = self.inner.announce_sched.write();
                if self.inner.announce_stagger_applied.load(Ordering::Acquire) {
                    // Late activation after startup batch: due immediately;
                    // poll_announce_due / next tick will fire.
                    sched
                        .entry(id)
                        .or_insert_with(|| TorrentAnnounce::fresh(Instant::now()));
                } else {
                    // Pre-batch bootstrap: placeholder far in the future so the
                    // 2s announce poll cannot race-fire before created_at stagger.
                    sched.entry(id).or_insert_with(|| {
                        let far = Instant::now() + Duration::from_secs(24 * 3600);
                        TorrentAnnounce::fresh(far)
                    });
                }
            }
            // Startup: batch_announce_hot assigns created_at DESC stagger.
            // Later: poll_announce_due picks up next_due=now entries.
        }
        Ok(())
    }
}
