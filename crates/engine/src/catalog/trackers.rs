//! Per-torrent tracker announce key and last-announce stats.
//!
//! Methods on [`Catalog`]. Column `want_start` means the user wants the torrent
//! **active in the swarm**.

use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};

use super::open::Catalog;
use crate::error::{Error, Result};

/// Payload for [`Catalog::record_tracker_announce`].
#[derive(Debug, Clone)]
pub struct TrackerAnnounceUpdate {
    pub seeders: Option<u32>,
    pub leechers: Option<u32>,
    pub interval_secs: Option<u32>,
    pub peers: Option<u32>,
    /// `"ok"` or truncated failure/error.
    pub status: String,
    /// When true, overwrite S/L/interval/peers; when false (failure), leave
    /// previous swarm stats and only refresh status + timestamp.
    pub success: bool,
}

impl Catalog {
    /// rtorrent-style announce key for this torrent (0 if missing / not migrated).
    pub fn tracker_key(&self, torrent_id: i64) -> Result<u32> {
        let k: Option<i64> = self
            .conn
            .query_row(
                "SELECT COALESCE(tracker_key, 0) FROM torrent WHERE id = ?1",
                params![torrent_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(k.unwrap_or(0) as u32)
    }

    /// Persist announce key (must be non-zero).
    pub fn set_tracker_key(&mut self, torrent_id: i64, key: u32) -> Result<()> {
        if key == 0 {
            return Err(Error::Msg("tracker_key must be non-zero".into()));
        }
        let n = self.conn.execute(
            "UPDATE torrent SET tracker_key = ?1 WHERE id = ?2",
            params![key as i64, torrent_id],
        )?;
        if n == 0 {
            return Err(Error::Msg(format!("torrent id {torrent_id} not found")));
        }
        Ok(())
    }

    /// Return stored key, or generate + persist if zero/missing (rtorrent parity).
    pub fn ensure_tracker_key(&mut self, torrent_id: i64) -> Result<u32> {
        let k = self.tracker_key(torrent_id)?;
        if k != 0 {
            return Ok(k);
        }
        let k = crate::tracker::generate_tracker_key();
        self.set_tracker_key(torrent_id, k)?;
        Ok(k)
    }

    /// Record last announce result for a tracker URL on this torrent.
    ///
    /// Matches by exact URL first; falls back to case-insensitive trim match.
    pub fn record_tracker_announce(
        &mut self,
        torrent_id: i64,
        url: &str,
        update: &TrackerAnnounceUpdate,
    ) -> Result<()> {
        record_tracker_announce_on(&self.conn, torrent_id, url, update)
    }
}

/// Shared by [`Catalog::record_tracker_announce`] and [`Catalog::persist_after_announce`].
pub(super) fn record_tracker_announce_on(
    conn: &Connection,
    torrent_id: i64,
    url: &str,
    update: &TrackerAnnounceUpdate,
) -> Result<()> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let status = if update.status.chars().count() > 200 {
        update.status.chars().take(200).collect::<String>()
    } else {
        update.status.clone()
    };

    let n = if update.success {
        conn.execute(
            "UPDATE tracker SET
               seeders = COALESCE(?1, seeders),
               leechers = COALESCE(?2, leechers),
               last_announce_at = ?3,
               last_interval = COALESCE(?4, last_interval),
               last_peers = COALESCE(?5, last_peers),
               last_status = ?6
             WHERE torrent_id = ?7 AND url = ?8",
            params![
                update.seeders.map(|n| n as i64),
                update.leechers.map(|n| n as i64),
                now,
                update.interval_secs.map(|n| n as i64),
                update.peers.map(|n| n as i64),
                &status,
                torrent_id,
                url,
            ],
        )?
    } else {
        conn.execute(
            "UPDATE tracker SET last_announce_at = ?1, last_status = ?2
             WHERE torrent_id = ?3 AND url = ?4",
            params![now, &status, torrent_id, url],
        )?
    };
    if n > 0 {
        return Ok(());
    }

    // Case-insensitive fallback (metainfo / DB may differ in case only).
    let url_lc = url.trim().to_ascii_lowercase();
    let id: Option<i64> = conn
        .query_row(
            "SELECT id FROM tracker WHERE torrent_id = ?1
             AND lower(trim(url)) = ?2 LIMIT 1",
            params![torrent_id, url_lc],
            |r| r.get(0),
        )
        .optional()?;
    let Some(tid) = id else {
        return Ok(()); // no matching row — ignore (manual peer path, etc.)
    };

    if update.success {
        conn.execute(
            "UPDATE tracker SET
               seeders = COALESCE(?1, seeders),
               leechers = COALESCE(?2, leechers),
               last_announce_at = ?3,
               last_interval = COALESCE(?4, last_interval),
               last_peers = COALESCE(?5, last_peers),
               last_status = ?6
             WHERE id = ?7",
            params![
                update.seeders.map(|n| n as i64),
                update.leechers.map(|n| n as i64),
                now,
                update.interval_secs.map(|n| n as i64),
                update.peers.map(|n| n as i64),
                &status,
                tid,
            ],
        )?;
    } else {
        conn.execute(
            "UPDATE tracker SET last_announce_at = ?1, last_status = ?2 WHERE id = ?3",
            params![now, &status, tid],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{Catalog, TorrentInsert};

    fn insert_with_tracker(cat: &mut Catalog, url: &str) -> i64 {
        let mut ih = [0u8; 20];
        ih[19] = 42;
        let meta = crate::metainfo::Metainfo {
            infohash: ih,
            name: "t".into(),
            piece_length: 16384,
            piece_count: 1,
            total_size: 1,
            pieces: vec![0u8; 20],
            files: vec![],
            is_multi_file: false,
            private: false,
            trackers: vec![(0, url.into())],
            announce: Some(url.into()),
        };
        let ins = TorrentInsert::from_metainfo(meta, "/tmp");
        cat.insert_torrent(&ins).unwrap().id()
    }

    #[test]
    fn record_announce_stats_and_detail() {
        let mut cat = Catalog::open_in_memory().unwrap();
        let url = "http://tracker.example/announce";
        let id = insert_with_tracker(&mut cat, url);

        cat.record_tracker_announce(
            id,
            url,
            &TrackerAnnounceUpdate {
                seeders: Some(10),
                leechers: Some(3),
                interval_secs: Some(1800),
                peers: Some(50),
                status: "ok".into(),
                success: true,
            },
        )
        .unwrap();

        let d = cat.get_torrent_detail(id).unwrap();
        assert_eq!(d.trackers.len(), 1);
        let t = &d.trackers[0];
        assert_eq!(t.seeders, Some(10));
        assert_eq!(t.leechers, Some(3));
        assert_eq!(t.last_interval, Some(1800));
        assert_eq!(t.last_peers, Some(50));
        assert_eq!(t.last_status.as_deref(), Some("ok"));
        assert!(t.last_announce_at.is_some());
        assert_eq!(d.swarm_sl(), (Some(10), Some(3)));

        // Failure keeps previous S/L, updates status.
        cat.record_tracker_announce(
            id,
            url,
            &TrackerAnnounceUpdate {
                seeders: None,
                leechers: None,
                interval_secs: None,
                peers: None,
                status: "timeout".into(),
                success: false,
            },
        )
        .unwrap();
        let d = cat.get_torrent_detail(id).unwrap();
        let t = &d.trackers[0];
        assert_eq!(t.seeders, Some(10));
        assert_eq!(t.leechers, Some(3));
        assert_eq!(t.last_status.as_deref(), Some("timeout"));
    }
}
