//! Delete tombstones: keep optimistically-deleted rows out of stale lists.
//!
//! The catalog reader runs on its own thread, so a `CatalogList` whose SQL
//! started *before* a delete commits can arrive *after* the `SoftDeleted` /
//! `Removed` confirm. Every `ListCatalog` request carries a generation (`seq`)
//! echoed back in the reply; a reply at or below the tombstone generation is
//! stale and must not resurrect the row. The first newer reply ran after the
//! delete committed, so the tombstone is dropped — the row is either truly
//! gone (nothing to hide) or legitimately restored (re-add of the same
//! infohash must show again).
//!
//! Two phases: while the delete is still queued (requested but not confirmed)
//! the id is hidden from *every* generation, because a list kicked after the
//! optimistic hide can still run its `SELECT` before the mutation commits —
//! its newer `seq` proves nothing about the commit. Only the confirm stamps
//! the current generation, from which point newer lists are provably fresh.

use std::collections::HashMap;

use seedchamp_engine::TorrentListRow;

/// Generation that hides the id from every reply (delete queued, unconfirmed).
const HIDE_FROM_ALL: u64 = u64::MAX;

/// Tracks optimistically-deleted ids across in-flight catalog lists.
#[derive(Debug, Default)]
pub(super) struct DeleteTombstones {
    /// Newest list generation kicked so far.
    sent: u64,
    /// Deleted id → newest kicked generation when confirmed, or
    /// [`HIDE_FROM_ALL`] while the delete is still queued.
    tomb: HashMap<i64, u64>,
}

impl DeleteTombstones {
    /// Record a kicked `ListCatalog` generation.
    /// Never raises existing stamps: a list kicked while a delete is queued
    /// can still read pre-commit state, so only the confirm may stamp it.
    pub(super) fn kicked(&mut self, seq: u64) {
        self.sent = self.sent.max(seq);
    }

    /// Hide `id` now (optimistic Ctrl-D / `:remove`). The delete is still
    /// queued, so hide from every generation until the confirm stamps it.
    pub(super) fn optimistic_remove(&mut self, id: i64) {
        self.tomb.insert(id, HIDE_FROM_ALL);
    }

    /// Delete confirmed: keep hiding `id` from stale in-flight lists.
    /// Stamps the newest kicked generation so only later lists count fresh.
    pub(super) fn confirmed(&mut self, id: i64) {
        self.tomb.insert(id, self.sent);
    }

    /// Delete failed: let the row come back on the next list.
    pub(super) fn failed(&mut self, id: i64) {
        self.tomb.remove(&id);
    }

    pub(super) fn is_empty(&self) -> bool {
        self.tomb.is_empty()
    }

    /// Strip rows hidden by tombstones at or above `seq`, then drop every
    /// tombstone this reply proves stale (`seq` ran after it was stamped).
    pub(super) fn apply(&mut self, rows: &mut Vec<TorrentListRow>, seq: u64) {
        if self.is_empty() {
            return;
        }
        rows.retain(|r| self.tomb.get(&r.id).is_none_or(|t| seq > *t));
        self.tomb.retain(|_, t| seq <= *t);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: i64) -> TorrentListRow {
        TorrentListRow {
            id,
            infohash_hex: format!("{id:040x}"),
            name: format!("t{id}"),
            total_size: 1,
            piece_count: 1,
            state: "stopped".into(),
            complete: false,
            want_start: false,
            uploaded: 0,
            downloaded: 0,
            data_root: None,
            have_count: 0,
            created_at: 0,
            error_msg: None,
        }
    }

    fn ids(rows: &[TorrentListRow]) -> Vec<i64> {
        rows.iter().map(|r| r.id).collect()
    }

    fn rows(ids: &[i64]) -> Vec<TorrentListRow> {
        ids.iter().map(|&id| row(id)).collect()
    }

    /// The reported bug: optimistic delete → confirm → a stale list (SQL ran
    /// before the commit) arrives *after* the confirm. The row must stay
    /// hidden so the cursor does not jump; the next fresh list clears state.
    #[test]
    fn stale_list_after_confirm_keeps_row_hidden() {
        let mut d = DeleteTombstones::default();
        d.kicked(10); // list gen 10 in flight while the row is still in SQLite
        d.optimistic_remove(7); // Ctrl-D hides #7, tombstone stamped @10

        // Stale gen-10 list arrives before the confirm: still hidden.
        let mut r = rows(&[7, 8]);
        d.apply(&mut r, 10);
        assert_eq!(ids(&r), vec![8]);

        d.confirmed(7); // SoftDeleted arrives — tombstone must survive this.
        d.kicked(11); // confirm handler re-kicks for catalog truth

        // Stale gen-10 list (pre-commit SQL) arrives AFTER the confirm.
        let mut r = rows(&[7, 8]);
        d.apply(&mut r, 10);
        assert_eq!(ids(&r), vec![8], "stale list resurrected the row");

        // Fresh gen-11 list: row truly gone; tombstone is garbage-collected.
        let mut r = rows(&[8]);
        d.apply(&mut r, 11);
        assert_eq!(ids(&r), vec![8]);
        assert!(d.is_empty());
    }

    /// Re-adding the same infohash restores the id; a newer list must show it
    /// instead of hiding it forever behind the tombstone.
    #[test]
    fn fresh_list_shows_restored_row_and_clears_tombstone() {
        let mut d = DeleteTombstones::default();
        d.kicked(10);
        d.optimistic_remove(7);
        d.confirmed(7);
        d.kicked(11);

        // Restore committed before the gen-11 SQL ran: row is back, legally.
        let mut r = rows(&[7, 8]);
        d.apply(&mut r, 11);
        assert_eq!(ids(&r), vec![7, 8]);
        assert!(d.is_empty());
    }

    #[test]
    fn failed_delete_lets_row_return() {
        let mut d = DeleteTombstones::default();
        d.kicked(10);
        d.optimistic_remove(7);
        d.failed(7); // SoftDeleteFailed: row was never deleted

        let mut r = rows(&[7, 8]);
        d.apply(&mut r, 10);
        assert_eq!(ids(&r), vec![7, 8]);
        assert!(d.is_empty());
    }

    /// A list kicked after the optimistic hide can still read pre-commit
    /// state: its newer `seq` must not show the row nor collect the tomb.
    #[test]
    fn queued_delete_hides_row_from_newer_lists() {
        let mut d = DeleteTombstones::default();
        d.kicked(10);
        d.optimistic_remove(7);
        // tick_refresh kicks another list while the mutation is still queued.
        d.kicked(11);
        let mut r = rows(&[7, 8]);
        d.apply(&mut r, 11);
        assert_eq!(ids(&r), vec![8]);
        assert!(!d.is_empty(), "unconfirmed tombstone must survive");

        // Confirm stamps the current generation; the queued-window list is
        // stale even though its seq is newer than the optimistic kick.
        d.confirmed(7);
        d.kicked(12); // confirm handler re-kicks for catalog truth
        let mut r = rows(&[7, 8]);
        d.apply(&mut r, 11);
        assert_eq!(ids(&r), vec![8], "queued-window list resurrected the row");

        // Fresh list: row truly gone; tombstone is garbage-collected.
        let mut r = rows(&[8]);
        d.apply(&mut r, 12);
        assert_eq!(ids(&r), vec![8]);
        assert!(d.is_empty());
    }

    #[test]
    fn tombstones_are_per_id_and_gc_independently() {
        let mut d = DeleteTombstones::default();
        d.kicked(10);
        d.optimistic_remove(7);
        d.optimistic_remove(8);
        // Only #7's delete has committed: stamp it; #8 stays hidden from all.
        d.confirmed(7);
        d.kicked(11);

        let mut r = rows(&[7, 8, 9]);
        d.apply(&mut r, 11);
        assert_eq!(ids(&r), vec![7, 9]);
        assert!(!d.is_empty());

        // #8's delete commits; the next fresh list clears its tombstone too
        // (row present = restored, row absent = gone — either way stop hiding).
        d.confirmed(8);
        d.kicked(12);
        let mut r = rows(&[8, 9]);
        d.apply(&mut r, 12);
        assert_eq!(ids(&r), vec![8, 9]);
        assert!(d.is_empty());
    }
}
