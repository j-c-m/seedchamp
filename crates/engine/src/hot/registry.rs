//! Tracker tier load and infohash → hot torrent map.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU32, AtomicU64};
use std::sync::Arc;

use parking_lot::RwLock;

use crate::catalog::{all_set_bitfield, empty_bitfield, Catalog};
use crate::error::{Error, Result};

use super::{HaveHub, HotTorrent, PieceState};

/// Load enabled trackers grouped by tier (for announce without re-opening the DB).
pub fn load_tracker_tiers(catalog: &Catalog, torrent_id: i64) -> Vec<(i64, Vec<String>)> {
    let mut stmt = match catalog.conn().prepare(
        "SELECT tier, url FROM tracker WHERE torrent_id = ?1 AND enabled = 1 ORDER BY tier, id",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let rows = match stmt.query_map(rusqlite::params![torrent_id], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
    }) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let mut tiers: Vec<(i64, Vec<String>)> = Vec::new();
    for row in rows.flatten() {
        let (tier, url) = row;
        if let Some(last) = tiers.last_mut() {
            if last.0 == tier {
                last.1.push(url);
                continue;
            }
        }
        tiers.push((tier, vec![url]));
    }
    tiers
}

/// Map infohash → hot torrent.
#[derive(Default)]
pub struct HotRegistry {
    by_hash: HashMap<[u8; 20], Arc<HotTorrent>>,
    pub(crate) by_id: HashMap<i64, Arc<HotTorrent>>,
}

impl HotRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.by_hash.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_hash.is_empty()
    }

    pub fn get(&self, infohash: &[u8; 20]) -> Option<Arc<HotTorrent>> {
        self.by_hash.get(infohash).cloned()
    }

    pub fn get_id(&self, id: i64) -> Option<Arc<HotTorrent>> {
        self.by_id.get(&id).cloned()
    }

    pub fn insert(&mut self, t: HotTorrent) {
        let t = Arc::new(t);
        self.by_hash.insert(t.infohash, t.clone());
        self.by_id.insert(t.id, t);
    }

    pub fn remove(&mut self, infohash: &[u8; 20]) {
        if let Some(t) = self.by_hash.remove(infohash) {
            self.by_id.remove(&t.id);
        }
    }

    /// Load torrent from catalog **without** touching the registry.
    /// Callers must not hold `registry` locks — this does SQLite I/O.
    pub fn load_from_catalog(
        catalog: &mut Catalog,
        torrent_id: i64,
        allow_incomplete: bool,
    ) -> Result<HotTorrent> {
        let layout = catalog.load_storage_layout(torrent_id)?;
        let (complete, bitfield, have_count) = catalog.load_bitfield_bytes(torrent_id)?;
        if !complete && !allow_incomplete {
            return Err(Error::Msg(format!(
                "torrent {torrent_id} not complete (have {have_count}); recheck first"
            )));
        }

        let (infohash, name, piece_count): (Vec<u8>, String, i64) = catalog.conn().query_row(
            "SELECT infohash, name, piece_count FROM torrent WHERE id = ?1",
            rusqlite::params![torrent_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        if infohash.len() != 20 {
            return Err(Error::Msg("bad infohash length".into()));
        }
        let mut ih = [0u8; 20];
        ih.copy_from_slice(&infohash);
        let pc = piece_count as u32;
        // Complete seeders never verify downloads — skip loading multi‑MB hash blob.
        let hash_arc = if complete {
            Arc::new(Vec::new())
        } else {
            let hashes = catalog.load_piece_hashes(torrent_id)?;
            if hashes.len() != pc as usize * 20 {
                return Err(Error::Msg(format!(
                    "piece hash blob size {} != {pc}*20",
                    hashes.len()
                )));
            }
            Arc::new(hashes)
        };

        let bf = if complete {
            all_set_bitfield(pc)
        } else if bitfield.is_empty() {
            empty_bitfield(pc)
        } else {
            bitfield
        };

        let file_priority = HotTorrent::priorities_from_layout(&layout);
        let tracker_tiers = load_tracker_tiers(catalog, torrent_id);
        // Stored tracker key, or generate and persist when missing.
        let tracker_key = catalog.ensure_tracker_key(torrent_id)?;
        Ok(HotTorrent {
            id: torrent_id,
            infohash: ih,
            name,
            layout: RwLock::new(Arc::new(layout)),
            piece_count: pc,
            piece_hashes: RwLock::new(hash_arc),
            pieces: RwLock::new(PieceState {
                bitfield: bf,
                complete,
                have_count: if complete { pc } else { have_count },
            }),
            file_priority: RwLock::new(file_priority),
            tracker_tiers,
            tracker_key,
            wanted_bf: RwLock::new(empty_bitfield(pc)),
            download_missing: AtomicU32::new(0),
            have_count_atomic: AtomicU32::new(if complete { pc } else { have_count }),
            completed_payload: AtomicU64::new(0),
            in_flight: RwLock::new(HashSet::new()),
            availability: RwLock::new(vec![0u16; pc as usize]),
            have_hub: HaveHub::new(),
            staging_pool: RwLock::new(None),
            staging_mem_limit: AtomicU64::new(crate::staging::DEFAULT_STAGING_MEM_LIMIT),
        }
        .finish_new())
    }

    /// Activate all complete torrents: load from catalog, then insert.
    ///
    /// Each torrent is loaded with [`Self::load_from_catalog`] before insert so
    /// SQLite I/O is not interleaved with other registry work for that row.
    pub fn activate_all_complete(&mut self, catalog: &mut Catalog) -> Result<usize> {
        let rows = catalog.list_torrents()?;
        let mut n = 0;
        for r in rows {
            if !r.complete {
                continue;
            }
            match Self::load_from_catalog(catalog, r.id, false) {
                Ok(hot) => {
                    self.insert(hot);
                    n += 1;
                }
                Err(e) => tracing::warn!(id = r.id, error = %e, "skip activate"),
            }
        }
        Ok(n)
    }

    /// Resolve torrent from MSE HASH(req2,SKEY) after deobfuscation.
    pub fn match_req2(&self, req2: &[u8; 20]) -> Option<Arc<HotTorrent>> {
        for t in self.by_hash.values() {
            if crate::crypto::hash_req2(&t.infohash) == *req2 {
                return Some(t.clone());
            }
        }
        None
    }

    /// Snapshot of hot torrent ids (for announce).
    pub fn ids(&self) -> Vec<i64> {
        self.by_id.keys().copied().collect()
    }
}
