//! Peer cache (compact addresses).
//!
//! Methods on [`Catalog`]. Column `want_start` means the user wants the torrent
//! **active in the swarm**.

use rusqlite::{params, Connection};

use super::open::Catalog;
use super::queries::{decode_peer_addr, encode_peer_addr, unix_now_secs};
use super::trackers::TrackerAnnounceUpdate;
use crate::error::Result;

impl Catalog {
    /// Upsert tracker/manual peers; bumps `last_seen` for each.
    pub fn upsert_peer_cache(
        &mut self,
        torrent_id: i64,
        peers: &[std::net::SocketAddr],
    ) -> Result<usize> {
        if peers.is_empty() {
            return Ok(0);
        }
        let now = unix_now_secs();
        let tx = self.conn.transaction()?;
        let n = upsert_peer_cache_on(&tx, torrent_id, peers, now)?;
        tx.commit()?;
        Ok(n)
    }

    /// Peers for dialing, most recently seen first (limit caps rows).
    pub fn list_peer_cache(
        &self,
        torrent_id: i64,
        limit: usize,
    ) -> Result<Vec<std::net::SocketAddr>> {
        let limit = limit.max(1) as i64;
        let mut stmt = self.conn.prepare(
            "SELECT addr FROM peer_cache
             WHERE torrent_id = ?1
             ORDER BY last_seen DESC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![torrent_id, limit], |r| {
            let blob: Vec<u8> = r.get(0)?;
            Ok(blob)
        })?;
        let mut out = Vec::new();
        for row in rows {
            let blob = row?;
            if let Some(addr) = decode_peer_addr(&blob) {
                out.push(addr);
            }
        }
        Ok(out)
    }

    /// How many peers are cached for this torrent.
    pub fn peer_cache_len(&self, torrent_id: i64) -> Result<usize> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM peer_cache WHERE torrent_id = ?1",
            params![torrent_id],
            |r| r.get(0),
        )?;
        Ok(n as usize)
    }

    /// One transaction after announce:
    /// 1. upsert tracker compact peers  
    /// 2. prune to `prune_keep` (tracker set only)  
    /// 3. upsert manual peers **after** prune so they always survive  
    /// 4. optional tracker announce stats  
    ///
    /// Restores pre-batch semantics: manuals are never competing with a full
    /// tracker list under a shared `last_seen` for prune.
    pub fn persist_after_announce(
        &mut self,
        torrent_id: i64,
        tracker_peers: &[std::net::SocketAddr],
        manual_peers: &[std::net::SocketAddr],
        prune_keep: usize,
        tracker: Option<(&str, &TrackerAnnounceUpdate)>,
    ) -> Result<()> {
        if tracker_peers.is_empty() && manual_peers.is_empty() && tracker.is_none() {
            return Ok(());
        }
        let now = unix_now_secs();
        let tx = self.conn.transaction()?;
        if !tracker_peers.is_empty() {
            upsert_peer_cache_on(&tx, torrent_id, tracker_peers, now)?;
            prune_peer_cache_on(&tx, torrent_id, prune_keep)?;
        }
        // After prune so --peer / config manuals remain in the cache even when
        // the tracker returned ≥ keep peers with the same last_seen.
        if !manual_peers.is_empty() {
            upsert_peer_cache_on(&tx, torrent_id, manual_peers, now)?;
        }
        if let Some((url, update)) = tracker {
            super::trackers::record_tracker_announce_on(&tx, torrent_id, url, update)?;
        }
        tx.commit()?;
        Ok(())
    }
}

fn upsert_peer_cache_on(
    conn: &Connection,
    torrent_id: i64,
    peers: &[std::net::SocketAddr],
    now: i64,
) -> Result<usize> {
    let mut stmt = conn.prepare(
        "INSERT INTO peer_cache (torrent_id, addr, last_seen, flags)
         VALUES (?1, ?2, ?3, 0)
         ON CONFLICT(torrent_id, addr) DO UPDATE SET
           last_seen = excluded.last_seen",
    )?;
    let mut n = 0usize;
    for addr in peers {
        let blob = encode_peer_addr(*addr);
        stmt.execute(params![torrent_id, blob, now])?;
        n += 1;
    }
    Ok(n)
}

/// Delete oldest peers when over `keep`. Cheap COUNT short-circuit when under cap.
///
/// Caller is inside a transaction (`persist_after_announce`).
fn prune_peer_cache_on(conn: &Connection, torrent_id: i64, keep: usize) -> Result<usize> {
    let keep = keep.max(1) as i64;
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM peer_cache WHERE torrent_id = ?1",
        params![torrent_id],
        |r| r.get(0),
    )?;
    if count <= keep {
        return Ok(0);
    }
    let n = conn.execute(
        "DELETE FROM peer_cache
         WHERE torrent_id = ?1
           AND addr NOT IN (
             SELECT addr FROM peer_cache
             WHERE torrent_id = ?1
             ORDER BY last_seen DESC
             LIMIT ?2
           )",
        params![torrent_id, keep],
    )?;
    Ok(n)
}
