//! Data roots, file rows, complete-storage audit.
//!
//! Methods on [`Catalog`]. Column `want_start` means the user wants the torrent
//! **active in the swarm**.

use std::path::PathBuf;

use rusqlite::params;

use super::open::Catalog;
use super::queries::{file_have_bytes, StorageAuditReport};
use super::types::FileRow;
use crate::disk::spans::{FileLayout, StorageLayout};
use crate::error::{Error, Result};

impl Catalog {
    /// Result of [`Self::audit_complete_storage`].
    ///
    /// `checked` = complete non-deleted torrents examined; `demoted` = failed
    /// size/existence check and were marked incomplete + stopped.
    pub fn audit_complete_storage(&mut self) -> Result<StorageAuditReport> {
        use crate::disk::check_complete_layout;

        let ids: Vec<i64> = {
            let mut stmt = self.conn.prepare(
                "SELECT id FROM torrent
                 WHERE complete != 0
                   AND COALESCE(deleted, 0) = 0
                 ORDER BY id",
            )?;
            let rows = stmt.query_map([], |r| r.get(0))?;
            let mut out = Vec::new();
            for id in rows {
                out.push(id?);
            }
            out
        };

        let mut demoted = 0usize;
        for id in &ids {
            let layout = match self.load_storage_layout(*id) {
                Ok(l) => l,
                Err(e) => {
                    // No layout / meta_path — cannot seed safely.
                    tracing::warn!(id, error = %e, "complete torrent layout load failed; demoting");
                    self.mark_incomplete_storage_problem(
                        *id,
                        &format!("storage layout error: {e}"),
                    )?;
                    demoted += 1;
                    continue;
                }
            };
            if let Some(problem) = check_complete_layout(&layout) {
                tracing::warn!(
                    id,
                    path = %problem.path.display(),
                    kind = ?problem.kind,
                    expected = problem.expected,
                    actual = ?problem.actual,
                    "complete torrent storage check failed; demoting"
                );
                self.mark_incomplete_storage_problem(*id, &problem.error_msg())?;
                demoted += 1;
            }
        }
        Ok(StorageAuditReport {
            checked: ids.len(),
            demoted,
        })
    }

    /// Clear complete + bitfield HAVE, stop torrent, set `error_msg` (no disk changes).
    pub fn mark_incomplete_storage_problem(
        &mut self,
        torrent_id: i64,
        error_msg: &str,
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        // Clear HAVE so we never seed after empty ensure_storage recreation.
        tx.execute(
            "UPDATE bitfield SET bits = NULL, have_count = 0 WHERE torrent_id = ?1",
            params![torrent_id],
        )?;
        if tx.changes() == 0 {
            tx.execute(
                "INSERT INTO bitfield (torrent_id, bits, have_count) VALUES (?1, NULL, 0)",
                params![torrent_id],
            )?;
        }
        tx.execute(
            "UPDATE torrent SET complete = 0, want_start = 0, state = 'stopped', error_msg = ?1
             WHERE id = ?2",
            params![error_msg, torrent_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn load_storage_layout(&self, torrent_id: i64) -> Result<StorageLayout> {
        let (piece_length, piece_count, total_size, data_root): (i64, i64, i64, String) =
            self.conn.query_row(
                "SELECT t.piece_length, t.piece_count, t.total_size, m.data_root
                 FROM torrent t
                 JOIN meta_path m ON m.torrent_id = t.id
                 WHERE t.id = ?1",
                params![torrent_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )?;

        let mut stmt = self.conn.prepare(
            "SELECT path, size, offset, priority FROM torrent_file
             WHERE torrent_id = ?1 ORDER BY idx",
        )?;
        let files = stmt
            .query_map(params![torrent_id], |r| {
                Ok(FileLayout {
                    path: PathBuf::from(r.get::<_, String>(0)?),
                    size: r.get::<_, i64>(1)? as u64,
                    offset: r.get::<_, i64>(2)? as u64,
                    priority: r.get::<_, i64>(3)? as i32,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        if files.is_empty() {
            return Err(Error::Msg(format!(
                "torrent {torrent_id} has no files in catalog"
            )));
        }

        Ok(StorageLayout {
            data_root: PathBuf::from(data_root),
            piece_length: piece_length as u32,
            piece_count: piece_count as u32,
            total_size: total_size as u64,
            files,
        })
    }

    /// Update `meta_path.data_root` for a torrent.
    pub fn set_data_root(&mut self, torrent_id: i64, data_root: &std::path::Path) -> Result<()> {
        let s = data_root.display().to_string();
        let n = self.conn.execute(
            "UPDATE meta_path SET data_root = ?1 WHERE torrent_id = ?2",
            params![s, torrent_id],
        )?;
        if n == 0 {
            return Err(Error::Msg(format!(
                "torrent {torrent_id} has no meta_path row"
            )));
        }
        Ok(())
    }

    /// Set permanent library root (leech_cache staging). Empty/None clears staging.
    pub fn set_home_root(
        &mut self,
        torrent_id: i64,
        home_root: Option<&std::path::Path>,
    ) -> Result<()> {
        let s = home_root.map(|p| p.display().to_string());
        let n = self.conn.execute(
            "UPDATE meta_path SET home_root = ?1 WHERE torrent_id = ?2",
            params![s, torrent_id],
        )?;
        if n == 0 {
            return Err(Error::Msg(format!(
                "torrent {torrent_id} has no meta_path row"
            )));
        }
        Ok(())
    }

    /// Switch from leech_cache to permanent root and clear staging marker.
    pub fn complete_leech_cache_handoff(
        &mut self,
        torrent_id: i64,
        permanent_root: &std::path::Path,
    ) -> Result<()> {
        let s = permanent_root.display().to_string();
        let n = self.conn.execute(
            "UPDATE meta_path SET data_root = ?1, home_root = NULL WHERE torrent_id = ?2",
            params![s, torrent_id],
        )?;
        if n == 0 {
            return Err(Error::Msg(format!(
                "torrent {torrent_id} has no meta_path row"
            )));
        }
        Ok(())
    }

    pub fn get_data_root(&self, torrent_id: i64) -> Result<PathBuf> {
        let s: String = self.conn.query_row(
            "SELECT data_root FROM meta_path WHERE torrent_id = ?1",
            params![torrent_id],
            |r| r.get(0),
        )?;
        Ok(PathBuf::from(s))
    }

    /// Permanent root when staged on leech_cache (`None` if not staged).
    pub fn get_home_root(&self, torrent_id: i64) -> Result<Option<PathBuf>> {
        let s: Option<String> = self.conn.query_row(
            "SELECT home_root FROM meta_path WHERE torrent_id = ?1",
            params![torrent_id],
            |r| r.get(0),
        )?;
        Ok(s.filter(|x| !x.is_empty()).map(PathBuf::from))
    }

    /// Torrents still staged on leech_cache (home_root set) for handoff / recovery.
    pub fn list_staged_leech_cache_ids(&self) -> Result<Vec<i64>> {
        let mut stmt = self.conn.prepare(
            "SELECT torrent_id FROM meta_path
             WHERE home_root IS NOT NULL AND TRIM(home_root) != ''
             ORDER BY torrent_id",
        )?;
        let ids = stmt
            .query_map([], |r| r.get::<_, i64>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    /// Sum of `torrent.total_size` for torrents staged on leech_cache (`home_root` set).
    ///
    /// Used as committed reservation for `paths.leech_cache_size` (not an on-disk walk).
    pub fn leech_cache_reserved_bytes(&self) -> Result<u64> {
        let sum: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(t.total_size), 0)
             FROM torrent t
             JOIN meta_path m ON m.torrent_id = t.id
             WHERE m.home_root IS NOT NULL AND TRIM(m.home_root) != ''
               AND t.deleted_at IS NULL",
            [],
            |r| r.get(0),
        )?;
        Ok(sum.max(0) as u64)
    }

    /// List files for a torrent (same fields as detail view).
    pub fn list_files(&self, torrent_id: i64) -> Result<Vec<FileRow>> {
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
        Ok(files)
    }

    /// Files with per-file completion (bytes present from verified pieces).
    pub fn list_files_progress(&self, torrent_id: i64) -> Result<Vec<super::FileProgress>> {
        let files = self.list_files(torrent_id)?;
        let (piece_length, piece_count, total_size): (i64, i64, i64) = self.conn.query_row(
            "SELECT piece_length, piece_count, total_size FROM torrent WHERE id = ?1",
            params![torrent_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let piece_length = piece_length as u64;
        let piece_count = piece_count as u32;
        let total_size = total_size as u64;
        let (complete, bits, _) = self.load_bitfield_bytes(torrent_id)?;

        let mut out = Vec::with_capacity(files.len());
        for file in files {
            let have_bytes = if complete || file.size == 0 {
                file.size
            } else if piece_length == 0 || piece_count == 0 {
                0
            } else {
                file_have_bytes(&file, piece_length, piece_count, total_size, &bits)
            };
            out.push(super::FileProgress { file, have_bytes });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod leech_cache_reserved_tests {
    use super::*;
    use rusqlite::params;

    #[test]
    fn reserved_sums_staged_total_size() {
        let mut cat = Catalog::open_in_memory().unwrap();
        cat.conn
            .execute(
                "INSERT INTO torrent (infohash, name, total_size, piece_length, piece_count, private, created_at)
                 VALUES (x'0000000000000000000000000000000000000001', 'a', 1000, 16384, 1, 0, 0)",
                [],
            )
            .unwrap();
        let id_a = cat.conn.last_insert_rowid();
        cat.conn
            .execute(
                "INSERT INTO meta_path (torrent_id, data_root, home_root) VALUES (?1, '/cache/a', '/home')",
                params![id_a],
            )
            .unwrap();

        cat.conn
            .execute(
                "INSERT INTO torrent (infohash, name, total_size, piece_length, piece_count, private, created_at)
                 VALUES (x'0000000000000000000000000000000000000002', 'b', 2500, 16384, 1, 0, 0)",
                [],
            )
            .unwrap();
        let id_b = cat.conn.last_insert_rowid();
        cat.conn
            .execute(
                "INSERT INTO meta_path (torrent_id, data_root, home_root) VALUES (?1, '/home/b', NULL)",
                params![id_b],
            )
            .unwrap();

        assert_eq!(cat.leech_cache_reserved_bytes().unwrap(), 1000);

        cat.conn
            .execute(
                "INSERT INTO torrent (infohash, name, total_size, piece_length, piece_count, private, created_at)
                 VALUES (x'0000000000000000000000000000000000000003', 'c', 400, 16384, 1, 0, 0)",
                [],
            )
            .unwrap();
        let id_c = cat.conn.last_insert_rowid();
        cat.conn
            .execute(
                "INSERT INTO meta_path (torrent_id, data_root, home_root) VALUES (?1, '/cache/c', '/home')",
                params![id_c],
            )
            .unwrap();
        assert_eq!(cat.leech_cache_reserved_bytes().unwrap(), 1400);

        cat.mark_deleted(id_a).unwrap();
        assert_eq!(cat.leech_cache_reserved_bytes().unwrap(), 400);
    }
}
