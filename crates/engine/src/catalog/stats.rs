//! Lifetime uploaded/downloaded counters.
//!
//! Methods on [`Catalog`]. Column `want_start` means the user wants the torrent
//! **active in the swarm**.

use rusqlite::{params, OptionalExtension};

use super::open::Catalog;
use crate::error::Result;

impl Catalog {
    /// Lifetime uploaded bytes from `stats` (0 if missing).
    pub fn stats_uploaded(&self, torrent_id: i64) -> Result<u64> {
        let v: Option<i64> = self
            .conn
            .query_row(
                "SELECT uploaded FROM stats WHERE torrent_id = ?1",
                params![torrent_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(v.unwrap_or(0).max(0) as u64)
    }

    /// Raise lifetime uploaded to at least `uploaded` (monotonic; safe to call often).
    pub fn set_uploaded_at_least(&mut self, torrent_id: i64, uploaded: u64) -> Result<()> {
        self.set_uploaded_batch(&[(torrent_id, uploaded)])
    }

    /// Raise lifetime uploaded for many torrents in **one transaction**.
    pub fn set_uploaded_batch(&mut self, rows: &[(i64, u64)]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let tx = self.conn.transaction()?;
        {
            let mut ins = tx.prepare(
                "INSERT OR IGNORE INTO stats (torrent_id, uploaded, downloaded, corrupted, active_time, finished_at)
                 VALUES (?1, 0, 0, 0, 0, NULL)",
            )?;
            let mut upd =
                tx.prepare("UPDATE stats SET uploaded = MAX(uploaded, ?1) WHERE torrent_id = ?2")?;
            for &(torrent_id, uploaded) in rows {
                ins.execute(params![torrent_id])?;
                upd.execute(params![uploaded as i64, torrent_id])?;
            }
        }
        tx.commit()?;
        Ok(())
    }
}
