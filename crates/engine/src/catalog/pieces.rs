//! Bitfield, piece hashes, mark-have.
//!
//! Methods on [`Catalog`]. Column `want_start` means the user wants the torrent
//! **active in the swarm**.

use rusqlite::params;

use super::open::Catalog;
use super::types::{all_set_bitfield, bitfield_size_bytes, TorrentInsert};
use crate::error::{Error, Result};

impl Catalog {
    pub fn load_piece_hashes(&self, torrent_id: i64) -> Result<Vec<u8>> {
        let hashes: Vec<u8> = self.conn.query_row(
            "SELECT hashes FROM piece_hashes WHERE torrent_id = ?1",
            params![torrent_id],
            |r| r.get(0),
        )?;
        Ok(hashes)
    }

    /// Persist recheck result: bitfield + complete + have_count + state.
    pub fn set_bitfield_from_recheck(
        &mut self,
        torrent_id: i64,
        piece_count: u32,
        have: &[bool],
    ) -> Result<()> {
        if have.len() != piece_count as usize {
            return Err(Error::Msg("have slice length mismatch".into()));
        }
        let have_count = have.iter().filter(|&&b| b).count() as u32;
        let complete = piece_count > 0 && have_count == piece_count;

        let bits = if complete {
            None
        } else {
            let mut bf = vec![0u8; bitfield_size_bytes(piece_count)];
            for (i, &h) in have.iter().enumerate() {
                if h {
                    bf[i / 8] |= 1 << (7 - (i % 8));
                }
            }
            Some(bf)
        };

        let tx = self.conn.transaction()?;
        tx.execute(
            "UPDATE bitfield SET bits = ?1, have_count = ?2 WHERE torrent_id = ?3",
            params![bits, have_count as i64, torrent_id],
        )?;
        // ensure row exists
        if tx.changes() == 0 {
            tx.execute(
                "INSERT INTO bitfield (torrent_id, bits, have_count) VALUES (?1,?2,?3)",
                params![torrent_id, bits, have_count as i64],
            )?;
        }

        let finished = if complete {
            Some(TorrentInsert::now_unix())
        } else {
            None
        };
        tx.execute(
            "UPDATE torrent SET complete = ?1, state = 'stopped', error_msg = NULL WHERE id = ?2",
            params![complete as i64, torrent_id],
        )?;
        if complete {
            tx.execute(
                "UPDATE stats SET finished_at = COALESCE(finished_at, ?1) WHERE torrent_id = ?2",
                params![finished, torrent_id],
            )?;
            // compact: clear bits when complete
            tx.execute(
                "UPDATE bitfield SET bits = NULL, have_count = ?1 WHERE torrent_id = ?2",
                params![have_count as i64, torrent_id],
            )?;
            let _ = all_set_bitfield(piece_count);
        }
        tx.commit()?;
        Ok(())
    }

    /// Load wire bitfield bytes (all-set when complete and bits NULL).
    pub fn load_bitfield_bytes(&self, torrent_id: i64) -> Result<(bool, Vec<u8>, u32)> {
        let (complete, bits, have, _pc) = self.load_bitfield_state(torrent_id)?;
        Ok((complete, bits, have))
    }

    /// `(complete, bits, have_count, piece_count)` — one SELECT (no double piece_count).
    fn load_bitfield_state(&self, torrent_id: i64) -> Result<(bool, Vec<u8>, u32, u32)> {
        let (complete, bits, have, pc): (i64, Option<Vec<u8>>, i64, i64) = self.conn.query_row(
            "SELECT t.complete, b.bits, COALESCE(b.have_count, 0), t.piece_count
                 FROM torrent t
                 LEFT JOIN bitfield b ON b.torrent_id = t.id
                 WHERE t.id = ?1",
            params![torrent_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        let pc = pc as u32;
        let complete = complete != 0;
        if complete {
            return Ok((true, super::types::all_set_bitfield(pc), pc, pc));
        }
        let bf = bits.unwrap_or_else(|| super::types::empty_bitfield(pc));
        Ok((false, bf, have as u32, pc))
    }

    /// Apply many piece-have events in **one transaction**.
    ///
    /// Groups by torrent: one bitfield load/store per torrent (not per piece),
    /// one stats bump, and torrent-row updates only on first have or complete.
    ///
    /// Returns torrent ids that **became** complete in this batch.
    pub fn mark_pieces_have_batch(&mut self, pieces: &[(i64, u32, u32)]) -> Result<Vec<i64>> {
        if pieces.is_empty() {
            return Ok(Vec::new());
        }

        // Preserve first-seen torrent order for stable status messages.
        let mut order: Vec<i64> = Vec::new();
        let mut by_tid: std::collections::HashMap<i64, Vec<(u32, u32)>> =
            std::collections::HashMap::new();
        for &(tid, index, plen) in pieces {
            by_tid
                .entry(tid)
                .or_insert_with(|| {
                    order.push(tid);
                    Vec::new()
                })
                .push((index, plen));
        }

        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let outcome = (|| -> Result<Vec<i64>> {
            let mut became_complete = Vec::new();
            for tid in order {
                let Some(items) = by_tid.remove(&tid) else {
                    continue;
                };
                let (complete, mut bits, mut have, pc) = self.load_bitfield_state(tid)?;
                if complete || pc == 0 {
                    continue;
                }
                let prev_have = have;
                let mut delta_down = 0i64;
                let mut changed = false;
                for (index, plen) in items {
                    if index >= pc || super::types::bitfield_get(&bits, index) {
                        continue;
                    }
                    super::types::bitfield_set(&mut bits, index);
                    have += 1;
                    delta_down += plen as i64;
                    changed = true;
                }
                if !changed {
                    continue;
                }

                let now_complete = have >= pc;
                let store_bits = if now_complete {
                    None
                } else {
                    Some(bits.as_slice())
                };
                self.conn.execute(
                    "UPDATE bitfield SET bits = ?1, have_count = ?2 WHERE torrent_id = ?3",
                    params![store_bits, have as i64, tid],
                )?;
                if self.conn.changes() == 0 {
                    self.conn.execute(
                        "INSERT INTO bitfield (torrent_id, bits, have_count) VALUES (?1,?2,?3)",
                        params![tid, store_bits, have as i64],
                    )?;
                }
                if delta_down > 0 {
                    self.conn.execute(
                        "UPDATE stats SET downloaded = downloaded + ?1 WHERE torrent_id = ?2",
                        params![delta_down, tid],
                    )?;
                }

                if now_complete {
                    let finished = super::types::TorrentInsert::now_unix();
                    self.conn.execute(
                        "UPDATE torrent SET complete = 1, state = 'started' WHERE id = ?1",
                        params![tid],
                    )?;
                    self.conn.execute(
                        "UPDATE stats SET finished_at = COALESCE(finished_at, ?1) WHERE torrent_id = ?2",
                        params![finished, tid],
                    )?;
                    self.conn.execute(
                        "UPDATE bitfield SET bits = NULL, have_count = ?1 WHERE torrent_id = ?2",
                        params![pc as i64, tid],
                    )?;
                    became_complete.push(tid);
                } else if prev_have == 0 {
                    // First cataloged have for this torrent — set state once.
                    // Later pieces skip torrent-row UPDATE (was every piece).
                    self.conn.execute(
                        "UPDATE torrent SET state = 'started', complete = 0 WHERE id = ?1",
                        params![tid],
                    )?;
                }
            }
            Ok(became_complete)
        })();

        match outcome {
            Ok(v) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(v)
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    // --- Peer cache (compact sockaddr blob, last_seen) ---
}
