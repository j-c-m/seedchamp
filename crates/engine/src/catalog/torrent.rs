//! Torrent row lifecycle, list/detail, soft-delete, resolve.
//!
//! Methods on [`Catalog`]. Column `want_start` means the user wants the torrent
//! **active in the swarm**.

use rusqlite::{params, OptionalExtension};

use super::open::Catalog;
use super::queries::{unix_now_secs, InsertOutcome};
use super::types::{FileRow, TorrentDetail, TorrentInsert, TorrentListRow, TrackerRow};
use crate::error::{Error, Result};

/// Map a row from [`Catalog::LIST_ROW_SQL`] column order.
fn list_row_from_sql(r: &rusqlite::Row<'_>) -> rusqlite::Result<TorrentListRow> {
    let ih: Vec<u8> = r.get(1)?;
    Ok(TorrentListRow {
        id: r.get(0)?,
        infohash_hex: hex::encode(ih),
        name: r.get(2)?,
        total_size: r.get::<_, i64>(3)? as u64,
        piece_count: r.get::<_, i64>(4)? as u32,
        state: r.get(5)?,
        complete: r.get::<_, i64>(6)? != 0,
        want_start: r.get::<_, i64>(7)? != 0,
        uploaded: r.get::<_, i64>(8)? as u64,
        downloaded: r.get::<_, i64>(9)? as u64,
        data_root: r.get(10)?,
        have_count: r.get::<_, i64>(11)? as u32,
        created_at: r.get(12)?,
        error_msg: r.get(13)?,
    })
}

impl Catalog {
    /// Insert torrent; returns row id. On infohash conflict, returns existing id (no overwrite).
    ///
    /// If the existing row was **soft-deleted** (`deleted=1`), restores it so watch /
    /// re-import / `add` make the torrent visible again ([`InsertOutcome::Restored`]).
    pub fn insert_torrent(&mut self, ins: &TorrentInsert) -> Result<InsertOutcome> {
        let existing: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM torrent WHERE infohash = ?1",
                params![&ins.metainfo.infohash[..]],
                |r| r.get(0),
            )
            .optional()?;

        if let Some(id) = existing {
            if self.is_deleted(id)? {
                self.restore_deleted(id, ins)?;
                return Ok(InsertOutcome::Restored { id });
            }
            return Ok(InsertOutcome::Exists { id });
        }

        let tx = self.conn.transaction()?;
        let created = ins
            .created_at
            .filter(|&t| t > 0)
            .unwrap_or_else(TorrentInsert::now_unix);
        let m = &ins.metainfo;

        let tracker_key = match ins.tracker_key.filter(|&k| k != 0) {
            Some(k) => k,
            None => crate::tracker::generate_tracker_key(),
        };
        tx.execute(
            "INSERT INTO torrent (
                infohash, name, piece_length, piece_count, total_size,
                state, want_start, complete, private, tracker_key, created_at, error_msg
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,NULL)",
            params![
                &m.infohash[..],
                m.name,
                m.piece_length as i64,
                m.piece_count as i64,
                m.total_size as i64,
                ins.state,
                ins.want_start as i64,
                ins.complete as i64,
                m.private as i64,
                tracker_key as i64,
                created,
            ],
        )?;
        let id = tx.last_insert_rowid();

        for (idx, f) in m.files.iter().enumerate() {
            let prio = ins.file_priorities.get(idx).copied().unwrap_or(1);
            tx.execute(
                "INSERT INTO torrent_file (torrent_id, idx, path, size, offset, priority)
                 VALUES (?1,?2,?3,?4,?5,?6)",
                params![
                    id,
                    idx as i64,
                    f.path.to_string_lossy().as_ref(),
                    f.size as i64,
                    f.offset as i64,
                    prio as i64,
                ],
            )?;
        }

        tx.execute(
            "INSERT INTO piece_hashes (torrent_id, hashes) VALUES (?1,?2)",
            params![id, &m.pieces],
        )?;

        let bits = if ins.complete && ins.bitfield.is_none() {
            None
        } else {
            ins.bitfield.clone()
        };
        tx.execute(
            "INSERT INTO bitfield (torrent_id, bits, have_count) VALUES (?1,?2,?3)",
            params![id, bits, ins.have_count as i64],
        )?;

        tx.execute(
            "INSERT INTO stats (torrent_id, uploaded, downloaded, corrupted, active_time, finished_at)
             VALUES (?1,?2,?3,0,0,?4)",
            params![
                id,
                ins.uploaded as i64,
                ins.downloaded as i64,
                ins.finished_at
            ],
        )?;

        for (tier, url) in &m.trackers {
            tx.execute(
                "INSERT INTO tracker (torrent_id, url, tier, enabled) VALUES (?1,?2,?3,1)",
                params![id, url, *tier as i64],
            )?;
        }

        tx.execute(
            "INSERT INTO meta_path (torrent_id, data_root, home_root, source_torrent) VALUES (?1,?2,?3,?4)",
            params![id, ins.data_root, ins.home_root, ins.source_torrent],
        )?;

        if let Some(ref blob) = ins.metainfo_blob {
            if !blob.is_empty() {
                tx.execute(
                    "INSERT INTO torrent_metainfo (torrent_id, blob) VALUES (?1,?2)",
                    params![id, blob.as_slice()],
                )?;
            }
        }

        tx.commit()?;
        Ok(InsertOutcome::Inserted { id })
    }

    /// Store or replace the exact original `.torrent` bytes for a torrent.
    pub fn set_metainfo_blob(&mut self, torrent_id: i64, blob: &[u8]) -> Result<()> {
        if blob.is_empty() {
            return Err(Error::Msg("empty metainfo blob".into()));
        }
        self.conn.execute(
            "INSERT INTO torrent_metainfo (torrent_id, blob) VALUES (?1,?2)
             ON CONFLICT(torrent_id) DO UPDATE SET blob = excluded.blob",
            params![torrent_id, blob],
        )?;
        Ok(())
    }

    /// Load original `.torrent` bytes if stored (perfect re-export).
    pub fn get_metainfo_blob(&self, torrent_id: i64) -> Result<Option<Vec<u8>>> {
        let v = self
            .conn
            .query_row(
                "SELECT blob FROM torrent_metainfo WHERE torrent_id = ?1",
                params![torrent_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(v)
    }

    /// Write stored metainfo blob to `path` (fails if no blob).
    pub fn export_torrent_file(&self, torrent_id: i64, path: &std::path::Path) -> Result<()> {
        let Some(blob) = self.get_metainfo_blob(torrent_id)? else {
            return Err(Error::Msg(format!(
                "torrent #{torrent_id} has no stored metainfo blob"
            )));
        };
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| Error::Path(parent.to_path_buf(), e.to_string()))?;
            }
        }
        std::fs::write(path, &blob).map_err(|e| Error::Path(path.to_path_buf(), e.to_string()))?;
        Ok(())
    }

    /// Refresh timestamps / lifetime stats from an rtorrent session re-import.
    ///
    /// Used when the torrent already exists so a second `import` can fix
    /// `created_at` / `finished_at` / uploaded / downloaded without deleting.
    pub fn update_import_meta(
        &mut self,
        id: i64,
        created_at: Option<i64>,
        finished_at: Option<i64>,
        uploaded: Option<u64>,
        downloaded: Option<u64>,
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        if let Some(t) = created_at.filter(|&t| t > 0) {
            tx.execute(
                "UPDATE torrent SET created_at = ?1 WHERE id = ?2",
                params![t, id],
            )?;
        }
        if finished_at.is_some() || uploaded.is_some() || downloaded.is_some() {
            // Ensure stats row exists.
            tx.execute(
                "INSERT OR IGNORE INTO stats (torrent_id, uploaded, downloaded, corrupted, active_time, finished_at)
                 VALUES (?1, 0, 0, 0, 0, NULL)",
                params![id],
            )?;
            if let Some(t) = finished_at.filter(|&t| t > 0) {
                tx.execute(
                    "UPDATE stats SET finished_at = ?1 WHERE torrent_id = ?2",
                    params![t, id],
                )?;
            }
            // Use MAX so a partial re-import never wipes higher catalog totals.
            if let Some(u) = uploaded {
                tx.execute(
                    "UPDATE stats SET uploaded = MAX(uploaded, ?1) WHERE torrent_id = ?2",
                    params![u as i64, id],
                )?;
            }
            if let Some(d) = downloaded {
                tx.execute(
                    "UPDATE stats SET downloaded = MAX(downloaded, ?1) WHERE torrent_id = ?2",
                    params![d as i64, id],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn list_torrents(&self) -> Result<Vec<TorrentListRow>> {
        self.list_torrents_filtered(None)
    }

    /// IDs with `want_start=1` and not soft-deleted (for hot-set sync; avoids full list).
    pub fn list_want_start_ids(&self) -> Result<Vec<i64>> {
        let mut stmt = self.conn.prepare(
            "SELECT id FROM torrent
             WHERE want_start != 0 AND COALESCE(deleted, 0) = 0
             ORDER BY id",
        )?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        let mut out = Vec::new();
        for id in rows {
            out.push(id?);
        }
        Ok(out)
    }

    /// Sort `ids` for **initial announce stagger**: `created_at DESC`, then `id DESC`.
    ///
    /// Unknown ids (not in catalog) sort last, by id DESC among themselves.
    pub fn order_ids_created_at_desc(&self, ids: &[i64]) -> Result<Vec<i64>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        use std::collections::HashMap;
        let mut created: HashMap<i64, i64> = HashMap::with_capacity(ids.len());
        {
            let mut stmt = self
                .conn
                .prepare("SELECT id, created_at FROM torrent WHERE id = ?1")?;
            for &id in ids {
                if let Ok((cid, ca)) = stmt.query_row(rusqlite::params![id], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
                }) {
                    created.insert(cid, ca);
                }
            }
        }
        let mut out = ids.to_vec();
        out.sort_by(|a, b| {
            let ca = created.get(a).copied().unwrap_or(i64::MIN);
            let cb = created.get(b).copied().unwrap_or(i64::MIN);
            cb.cmp(&ca).then_with(|| b.cmp(a))
        });
        Ok(out)
    }

    /// Shared SELECT for list rows (joins stats / meta_path / bitfield).
    const LIST_ROW_SQL: &str =
        "SELECT t.id, t.infohash, t.name, t.total_size, t.piece_count, t.state,
                    t.complete, t.want_start,
                    COALESCE(s.uploaded,0), COALESCE(s.downloaded,0),
                    m.data_root,
                    COALESCE(b.have_count, CASE WHEN t.complete != 0 THEN t.piece_count ELSE 0 END),
                    t.created_at,
                    t.error_msg
             FROM torrent t
             LEFT JOIN stats s ON s.torrent_id = t.id
             LEFT JOIN meta_path m ON m.torrent_id = t.id
             LEFT JOIN bitfield b ON b.torrent_id = t.id";

    /// List torrents; optional case-insensitive name/infohash substring filter.
    pub fn list_torrents_filtered(&self, filter: Option<&str>) -> Result<Vec<TorrentListRow>> {
        let sql = format!(
            "{} WHERE COALESCE(t.deleted, 0) = 0 ORDER BY t.id",
            Self::LIST_ROW_SQL
        );
        let mut stmt = self.conn.prepare(&sql)?;

        let rows = stmt.query_map([], list_row_from_sql)?;

        let mut out = Vec::new();
        let filt = filter
            .map(|s| s.to_ascii_lowercase())
            .filter(|s| !s.is_empty());
        for row in rows {
            let row = row?;
            if let Some(ref f) = filt {
                let name_ok = row.name.to_ascii_lowercase().contains(f);
                let ih_ok = row.infohash_hex.contains(f.as_str());
                let id_ok = row.id.to_string() == *f;
                if !(name_ok || ih_ok || id_ok) {
                    continue;
                }
            }
            out.push(row);
        }
        Ok(out)
    }

    /// One list row by id (non-deleted). Used by detail — never scans the full catalog.
    pub fn list_torrent_by_id(&self, torrent_id: i64) -> Result<Option<TorrentListRow>> {
        let sql = format!(
            "{} WHERE t.id = ?1 AND COALESCE(t.deleted, 0) = 0",
            Self::LIST_ROW_SQL
        );
        self.conn
            .query_row(&sql, params![torrent_id], list_row_from_sql)
            .optional()
            .map_err(Into::into)
    }

    pub fn get_torrent_detail(&self, torrent_id: i64) -> Result<TorrentDetail> {
        let list = self
            .list_torrent_by_id(torrent_id)?
            .ok_or_else(|| Error::Msg(format!("torrent id {torrent_id} not found")))?;

        let (piece_length, private, error_msg): (i64, i64, Option<String>) = self.conn.query_row(
            "SELECT piece_length, private, error_msg FROM torrent WHERE id = ?1",
            params![torrent_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;

        let source_torrent: Option<String> = self
            .conn
            .query_row(
                "SELECT source_torrent FROM meta_path WHERE torrent_id = ?1",
                params![torrent_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();

        let (corrupted, finished_at): (i64, Option<i64>) = self
            .conn
            .query_row(
                "SELECT COALESCE(corrupted,0), finished_at FROM stats WHERE torrent_id = ?1",
                params![torrent_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .unwrap_or((0, None));

        let mut fstmt = self.conn.prepare(
            "SELECT idx, path, size, offset, priority FROM torrent_file
             WHERE torrent_id = ?1 ORDER BY idx",
        )?;
        let files = fstmt
            .query_map(params![torrent_id], |r| {
                Ok(FileRow {
                    idx: r.get::<_, i64>(0)? as u32,
                    path: r.get(1)?,
                    size: r.get::<_, i64>(2)? as u64,
                    offset: r.get::<_, i64>(3)? as u64,
                    priority: r.get(4)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        let mut tstmt = self.conn.prepare(
            "SELECT id, url, tier, enabled,
                    seeders, leechers, last_announce_at, last_interval, last_peers, last_status
             FROM tracker
             WHERE torrent_id = ?1 ORDER BY tier, id",
        )?;
        let trackers = tstmt
            .query_map(params![torrent_id], |r| {
                let seeders: Option<i64> = r.get(4)?;
                let leechers: Option<i64> = r.get(5)?;
                let last_interval: Option<i64> = r.get(7)?;
                let last_peers: Option<i64> = r.get(8)?;
                Ok(TrackerRow {
                    id: r.get(0)?,
                    url: r.get(1)?,
                    tier: r.get::<_, i64>(2)? as u32,
                    enabled: r.get::<_, i64>(3)? != 0,
                    seeders: seeders.map(|n| n.max(0) as u32),
                    leechers: leechers.map(|n| n.max(0) as u32),
                    last_announce_at: r.get(6)?,
                    last_interval: last_interval.map(|n| n.max(0) as u32),
                    last_peers: last_peers.map(|n| n.max(0) as u32),
                    last_status: r.get(9)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(TorrentDetail {
            list,
            piece_length: piece_length as u32,
            private: private != 0,
            error_msg,
            source_torrent,
            corrupted: corrupted as u64,
            finished_at,
            files,
            trackers,
        })
    }

    pub fn set_want_start(&mut self, torrent_id: i64, want: bool) -> Result<()> {
        if want && self.is_deleted(torrent_id)? {
            return Err(Error::Msg(format!(
                "torrent #{torrent_id} is deleted; cannot start"
            )));
        }
        let state = if want { "started" } else { "stopped" };
        self.conn.execute(
            "UPDATE torrent SET want_start = ?1, state = ?2 WHERE id = ?3 AND COALESCE(deleted, 0) = 0",
            params![want as i64, state, torrent_id],
        )?;
        Ok(())
    }

    /// Align `state` with `want_start` after manual SQL / partial updates.
    ///
    /// `want_start=1` without `state='started'` is a common footgun; the engine
    /// activates on `want_start` alone while the TUI still shows `state`.
    pub fn sync_state_with_want_start(&mut self) -> Result<usize> {
        let n = self.conn.execute(
            "UPDATE torrent SET state = 'started'
             WHERE want_start != 0
               AND COALESCE(deleted, 0) = 0
               AND state NOT IN ('started', 'checking')",
            [],
        )?;
        let n2 = self.conn.execute(
            "UPDATE torrent SET state = 'stopped'
             WHERE want_start = 0
               AND COALESCE(deleted, 0) = 0
               AND state = 'started'",
            [],
        )?;
        Ok(n + n2)
    }

    pub fn is_deleted(&self, torrent_id: i64) -> Result<bool> {
        let d: Option<i64> = self
            .conn
            .query_row(
                "SELECT COALESCE(deleted, 0) FROM torrent WHERE id = ?1",
                params![torrent_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(d.unwrap_or(0) != 0)
    }

    /// Soft-delete: hide from lists; keep payload and catalog rows.
    ///
    /// Requires the torrent to be stopped (`want_start = 0`). Does **not**
    /// remove files on disk or hard-delete DB rows. Records `deleted_at` for
    /// later catalog-only purge ([`Self::purge_soft_deleted`]).
    pub fn mark_deleted(&mut self, torrent_id: i64) -> Result<()> {
        let row: Option<(i64, i64)> = self
            .conn
            .query_row(
                "SELECT want_start, COALESCE(deleted, 0) FROM torrent WHERE id = ?1",
                params![torrent_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((want_start, deleted)) = row else {
            return Err(Error::Msg(format!("torrent id {torrent_id} not found")));
        };
        if deleted != 0 {
            return Ok(()); // already deleted
        }
        if want_start != 0 {
            return Err(Error::Msg(format!("torrent #{torrent_id} is started")));
        }
        let now = unix_now_secs();
        self.conn.execute(
            "UPDATE torrent SET deleted = 1, deleted_at = ?1, want_start = 0, state = 'deleted' WHERE id = ?2",
            params![now, torrent_id],
        )?;
        Ok(())
    }

    /// Undo soft-delete (re-import / watch / add of the same infohash).
    ///
    /// Keeps bitfield, stats, and files; clears `deleted` / `deleted_at`, applies
    /// `want_start` / state from the new insert, refreshes data_root / source /
    /// metainfo blob.
    pub fn restore_deleted(&mut self, torrent_id: i64, ins: &TorrentInsert) -> Result<()> {
        if !self.is_deleted(torrent_id)? {
            return Ok(());
        }
        let state = if ins.want_start {
            "started"
        } else if ins.state == "deleted" || ins.state.is_empty() {
            "stopped"
        } else {
            ins.state.as_str()
        };
        self.conn.execute(
            "UPDATE torrent SET deleted = 0, deleted_at = NULL, want_start = ?1, state = ?2, error_msg = NULL WHERE id = ?3",
            params![ins.want_start as i64, state, torrent_id],
        )?;
        if !ins.data_root.is_empty() {
            self.conn.execute(
                "UPDATE meta_path SET data_root = ?1,
                     home_root = ?2,
                     source_torrent = COALESCE(?3, source_torrent)
                 WHERE torrent_id = ?4",
                params![ins.data_root, ins.home_root, ins.source_torrent, torrent_id],
            )?;
            // meta_path may be missing on very old rows — insert if needed.
            if self.conn.changes() == 0 {
                self.conn.execute(
                    "INSERT OR IGNORE INTO meta_path (torrent_id, data_root, home_root, source_torrent)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![
                        torrent_id,
                        ins.data_root,
                        ins.home_root,
                        ins.source_torrent
                    ],
                )?;
            }
        }
        if let Some(ref blob) = ins.metainfo_blob {
            if !blob.is_empty() {
                self.set_metainfo_blob(torrent_id, blob)?;
            }
        }
        Ok(())
    }

    /// Delete torrent and related rows (CASCADE).
    ///
    /// Catalog only — does **not** remove payload files under `data_root`.
    /// Requires the torrent to be stopped (`want_start = 0`), same as
    /// [`Self::mark_deleted`].
    pub fn remove_torrent(&mut self, torrent_id: i64) -> Result<()> {
        let row: Option<(i64, i64)> = self
            .conn
            .query_row(
                "SELECT want_start, COALESCE(deleted, 0) FROM torrent WHERE id = ?1",
                params![torrent_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((want_start, _deleted)) = row else {
            return Err(Error::Msg(format!("torrent id {torrent_id} not found")));
        };
        if want_start != 0 {
            return Err(Error::Msg(format!("torrent #{torrent_id} is started")));
        }
        let n = self
            .conn
            .execute("DELETE FROM torrent WHERE id = ?1", params![torrent_id])?;
        if n == 0 {
            return Err(Error::Msg(format!("torrent id {torrent_id} not found")));
        }
        Ok(())
    }

    /// Hard-remove soft-deleted catalog rows older than `days` (by `deleted_at`).
    ///
    /// - `days == 0` → no-op (disabled).
    /// - Never deletes payload / download files on disk — only SQLite rows
    ///   (CASCADE to files, bitfield, trackers, peer_cache, etc.).
    ///
    /// Returns the number of torrents purged.
    pub fn purge_soft_deleted(&mut self, days: u64) -> Result<usize> {
        if days == 0 {
            return Ok(0);
        }
        let now = unix_now_secs();
        let window = (days as i64).saturating_mul(86_400);
        let cutoff = now.saturating_sub(window);
        let ids: Vec<i64> = {
            let mut stmt = self.conn.prepare(
                "SELECT id FROM torrent
                 WHERE COALESCE(deleted, 0) != 0
                   AND deleted_at IS NOT NULL
                   AND deleted_at <= ?1
                 ORDER BY id",
            )?;
            let rows = stmt.query_map(params![cutoff], |r| r.get(0))?;
            let mut out = Vec::new();
            for id in rows {
                out.push(id?);
            }
            out
        };
        let mut n = 0usize;
        for id in ids {
            self.remove_torrent(id)?;
            n += 1;
        }
        Ok(n)
    }

    pub fn get_by_infohash(&self, infohash: &[u8; 20]) -> Result<Option<i64>> {
        let id = self
            .conn
            .query_row(
                "SELECT id FROM torrent WHERE infohash = ?1",
                params![&infohash[..]],
                |r| r.get(0),
            )
            .optional()?;
        Ok(id)
    }

    /// Resolve torrent id from decimal id or hex infohash (full 40 or unique prefix ≥4).
    pub fn resolve_torrent_ref(&self, spec: &str) -> Result<i64> {
        if let Ok(id) = spec.parse::<i64>() {
            let exists: Option<i64> = self
                .conn
                .query_row("SELECT id FROM torrent WHERE id = ?1", params![id], |r| {
                    r.get(0)
                })
                .optional()?;
            if exists.is_some() {
                return Ok(id);
            }
        }
        let hex = spec.trim().to_ascii_lowercase();
        if hex.len() >= 4 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
            // SQLite hex() is uppercase; lower(hex(...)) matches a lowercased prefix.
            let mut stmt = self
                .conn
                .prepare("SELECT id FROM torrent WHERE lower(hex(infohash)) LIKE ?1 || '%'")?;
            let rows: Vec<i64> = stmt
                .query_map(params![hex], |r| r.get(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            match rows.len() {
                1 => return Ok(rows[0]),
                0 => return Err(Error::Msg(format!("no torrent matching {spec:?}"))),
                n => {
                    return Err(Error::Msg(format!(
                        "ambiguous infohash prefix {spec:?} ({n} matches)"
                    )));
                }
            }
        }
        Err(Error::Msg(format!("invalid torrent ref {spec:?}")))
    }

    pub fn set_torrent_state(&mut self, torrent_id: i64, state: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE torrent SET state = ?1 WHERE id = ?2",
            params![state, torrent_id],
        )?;
        Ok(())
    }

    /// Set per-file download priority (`0` = off, `≥1` = on / normal).
    pub fn set_file_priority(
        &mut self,
        torrent_id: i64,
        file_idx: u32,
        priority: i32,
    ) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE torrent_file SET priority = ?1 WHERE torrent_id = ?2 AND idx = ?3",
            params![priority as i64, torrent_id, file_idx as i64],
        )?;
        if n == 0 {
            return Err(Error::Msg(format!(
                "torrent {torrent_id} has no file idx={file_idx}"
            )));
        }
        Ok(())
    }

    /// Set priorities for all files (index = position in `priorities`). Used by session import.
    pub fn set_file_priorities(&mut self, torrent_id: i64, priorities: &[i32]) -> Result<()> {
        let tx = self.conn.transaction()?;
        for (idx, &prio) in priorities.iter().enumerate() {
            tx.execute(
                "UPDATE torrent_file SET priority = ?1 WHERE torrent_id = ?2 AND idx = ?3",
                params![prio as i64, torrent_id, idx as i64],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod announce_order_tests {
    use super::*;
    use rusqlite::params;

    fn insert_at(cat: &Catalog, name: &str, ih_last: u8, created_at: i64) -> i64 {
        let mut ih = [0u8; 20];
        ih[19] = ih_last;
        cat.conn
            .execute(
                "INSERT INTO torrent (infohash, name, total_size, piece_length, piece_count, private, created_at)
                 VALUES (?1, ?2, 1, 16384, 1, 0, ?3)",
                params![&ih[..], name, created_at],
            )
            .unwrap();
        cat.conn.last_insert_rowid()
    }

    #[test]
    fn order_ids_created_at_desc_newest_first() {
        let cat = Catalog::open_in_memory().unwrap();
        let old = insert_at(&cat, "old", 1, 100);
        let mid = insert_at(&cat, "mid", 2, 200);
        let new = insert_at(&cat, "new", 3, 300);
        // Input scrambled
        let ordered = cat.order_ids_created_at_desc(&[mid, old, new]).unwrap();
        assert_eq!(ordered, vec![new, mid, old]);
    }

    #[test]
    fn list_torrent_by_id_and_detail_no_full_scan() {
        let cat = Catalog::open_in_memory().unwrap();
        let a = insert_at(&cat, "alpha", 1, 100);
        let b = insert_at(&cat, "beta", 2, 200);
        cat.conn
            .execute(
                "INSERT INTO tracker (torrent_id, url, tier, enabled) VALUES (?1, 'http://t/a', 0, 1)",
                params![a],
            )
            .unwrap();

        let row = cat.list_torrent_by_id(a).unwrap().expect("alpha");
        assert_eq!(row.id, a);
        assert_eq!(row.name, "alpha");
        assert!(cat.list_torrent_by_id(999_999).unwrap().is_none());

        let d = cat.get_torrent_detail(a).unwrap();
        assert_eq!(d.list.id, a);
        assert_eq!(d.trackers.len(), 1);
        assert_eq!(d.trackers[0].url, "http://t/a");

        // Other id still works independently.
        let d2 = cat.get_torrent_detail(b).unwrap();
        assert_eq!(d2.list.name, "beta");
        assert!(d2.trackers.is_empty());
    }
}
