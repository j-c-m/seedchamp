//! Shared catalog query types and helpers.

use super::types::{bitfield_get, FileRow};

/// Outcome of [`super::Catalog::audit_complete_storage`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StorageAuditReport {
    /// Complete, non-deleted torrents examined.
    pub checked: usize,
    /// Failed existence/size check; marked incomplete and stopped.
    pub demoted: usize,
}

/// Compact BT-style peer address: IPv4 = 6 bytes, IPv6 = 18 bytes.
pub fn encode_peer_addr(addr: std::net::SocketAddr) -> Vec<u8> {
    match addr {
        std::net::SocketAddr::V4(a) => {
            let mut v = Vec::with_capacity(6);
            v.extend_from_slice(&a.ip().octets());
            v.extend_from_slice(&a.port().to_be_bytes());
            v
        }
        std::net::SocketAddr::V6(a) => {
            let mut v = Vec::with_capacity(18);
            v.extend_from_slice(&a.ip().octets());
            v.extend_from_slice(&a.port().to_be_bytes());
            v
        }
    }
}

/// Inverse of [`encode_peer_addr`].
pub fn decode_peer_addr(blob: &[u8]) -> Option<std::net::SocketAddr> {
    match blob.len() {
        6 => {
            let ip = std::net::Ipv4Addr::new(blob[0], blob[1], blob[2], blob[3]);
            let port = u16::from_be_bytes([blob[4], blob[5]]);
            Some(std::net::SocketAddr::from((ip, port)))
        }
        18 => {
            let mut oct = [0u8; 16];
            oct.copy_from_slice(&blob[..16]);
            let port = u16::from_be_bytes([blob[16], blob[17]]);
            Some(std::net::SocketAddr::from((
                std::net::Ipv6Addr::from(oct),
                port,
            )))
        }
        _ => None,
    }
}

pub(crate) fn unix_now_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Bytes of `file` covered by pieces set in `bits`.
pub(crate) fn file_have_bytes(
    file: &FileRow,
    piece_length: u64,
    piece_count: u32,
    total_size: u64,
    bits: &[u8],
) -> u64 {
    if file.size == 0 || piece_length == 0 {
        return 0;
    }
    let file_start = file.offset;
    let file_end = file.offset.saturating_add(file.size);
    let first = (file_start / piece_length) as u32;
    let last = ((file_end.saturating_sub(1)) / piece_length) as u32;
    let last = last.min(piece_count.saturating_sub(1));
    let mut have = 0u64;
    for pi in first..=last {
        if !bitfield_get(bits, pi) {
            continue;
        }
        let pstart = pi as u64 * piece_length;
        let pend = (pstart + piece_length).min(total_size);
        let lo = pstart.max(file_start);
        let hi = pend.min(file_end);
        if hi > lo {
            have += hi - lo;
        }
    }
    have.min(file.size)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertOutcome {
    Inserted {
        id: i64,
    },
    /// Same infohash already present and not soft-deleted.
    Exists {
        id: i64,
    },
    /// Soft-deleted row restored (re-add / watch / import of same infohash).
    Restored {
        id: i64,
    },
}

impl InsertOutcome {
    pub fn id(self) -> i64 {
        match self {
            InsertOutcome::Inserted { id }
            | InsertOutcome::Exists { id }
            | InsertOutcome::Restored { id } => id,
        }
    }
}

#[cfg(test)]
mod peer_cache_tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};

    use rusqlite::params;

    use crate::catalog::Catalog;

    #[test]
    fn encode_decode_v4_v6() {
        let v4 = SocketAddr::from((Ipv4Addr::new(1, 2, 3, 4), 6881));
        let blob = encode_peer_addr(v4);
        assert_eq!(blob.len(), 6);
        assert_eq!(decode_peer_addr(&blob), Some(v4));

        let v6 = SocketAddr::from((Ipv6Addr::LOCALHOST, 51413));
        let blob = encode_peer_addr(v6);
        assert_eq!(blob.len(), 18);
        assert_eq!(decode_peer_addr(&blob), Some(v6));
        assert!(decode_peer_addr(&[0u8; 3]).is_none());
    }

    #[test]
    fn upsert_list_prune() {
        let mut cat = Catalog::open_in_memory().unwrap();
        // Minimal torrent row for FK.
        cat.conn
            .execute(
                "INSERT INTO torrent (infohash, name, total_size, piece_length, piece_count, private, created_at)
                 VALUES (x'0000000000000000000000000000000000000001', 't', 1, 16384, 1, 0, 0)",
                [],
            )
            .unwrap();
        let tid = 1i64;
        let a = SocketAddr::from((Ipv4Addr::new(9, 9, 9, 1), 1));
        let b = SocketAddr::from((Ipv4Addr::new(9, 9, 9, 2), 2));
        let c = SocketAddr::from((Ipv4Addr::new(9, 9, 9, 3), 3));
        cat.upsert_peer_cache(tid, &[a, b, c]).unwrap();
        assert_eq!(cat.peer_cache_len(tid).unwrap(), 3);
        let list = cat.list_peer_cache(tid, 10).unwrap();
        assert_eq!(list.len(), 3);
        assert!(list.contains(&a) && list.contains(&b) && list.contains(&c));
        // Re-upsert same set bumps last_seen (still 3 rows).
        cat.upsert_peer_cache(tid, &[a]).unwrap();
        assert_eq!(cat.peer_cache_len(tid).unwrap(), 3);
        // Announce persist prunes tracker peers to keep=2.
        cat.persist_after_announce(tid, &[a, b, c], &[], 2, None)
            .unwrap();
        assert_eq!(cat.peer_cache_len(tid).unwrap(), 2);
    }

    #[test]
    fn persist_after_announce_one_txn() {
        let mut cat = Catalog::open_in_memory().unwrap();
        cat.conn
            .execute(
                "INSERT INTO torrent (infohash, name, total_size, piece_length, piece_count, private, created_at)
                 VALUES (x'0000000000000000000000000000000000000002', 't2', 1, 16384, 1, 0, 0)",
                [],
            )
            .unwrap();
        cat.conn
            .execute(
                "INSERT INTO tracker (torrent_id, url, tier, enabled) VALUES (1, 'http://t/announce', 0, 1)",
                [],
            )
            .unwrap();
        // torrent id may not be 1 if autoincrement — query it
        let tid: i64 = cat
            .conn
            .query_row("SELECT id FROM torrent WHERE name = 't2'", [], |r| r.get(0))
            .unwrap();
        // Fix tracker FK to real tid
        cat.conn.execute("DELETE FROM tracker", []).unwrap();
        cat.conn
            .execute(
                "INSERT INTO tracker (torrent_id, url, tier, enabled) VALUES (?1, 'http://t/announce', 0, 1)",
                params![tid],
            )
            .unwrap();

        let a = SocketAddr::from((Ipv4Addr::new(1, 1, 1, 1), 1));
        let b = SocketAddr::from((Ipv4Addr::new(1, 1, 1, 2), 2));
        let c = SocketAddr::from((Ipv4Addr::new(1, 1, 1, 3), 3));
        let update = crate::catalog::TrackerAnnounceUpdate {
            seeders: Some(5),
            leechers: Some(1),
            interval_secs: Some(1800),
            peers: Some(3),
            status: "ok".into(),
            success: true,
        };
        cat.persist_after_announce(
            tid,
            &[a, b, c],
            &[],
            2,
            Some(("http://t/announce", &update)),
        )
        .unwrap();
        assert_eq!(cat.peer_cache_len(tid).unwrap(), 2);
        let d = cat.get_torrent_detail(tid).unwrap();
        assert_eq!(d.trackers[0].seeders, Some(5));
        assert_eq!(d.trackers[0].last_status.as_deref(), Some("ok"));
    }

    #[test]
    fn persist_after_announce_manuals_survive_prune() {
        let mut cat = Catalog::open_in_memory().unwrap();
        cat.conn
            .execute(
                "INSERT INTO torrent (infohash, name, total_size, piece_length, piece_count, private, created_at)
                 VALUES (x'0000000000000000000000000000000000000003', 't3', 1, 16384, 1, 0, 0)",
                [],
            )
            .unwrap();
        let tid: i64 = cat
            .conn
            .query_row("SELECT id FROM torrent WHERE name = 't3'", [], |r| r.get(0))
            .unwrap();

        // 5 tracker peers, prune keep=2, plus 2 manuals — manuals must remain.
        let tracker: Vec<SocketAddr> = (1..=5)
            .map(|i| SocketAddr::from((Ipv4Addr::new(10, 0, 0, i), 6881)))
            .collect();
        let m1 = SocketAddr::from((Ipv4Addr::new(192, 168, 1, 1), 51413));
        let m2 = SocketAddr::from((Ipv4Addr::new(192, 168, 1, 2), 51413));
        cat.persist_after_announce(tid, &tracker, &[m1, m2], 2, None)
            .unwrap();
        // 2 tracker survivors + 2 manuals
        assert_eq!(cat.peer_cache_len(tid).unwrap(), 4);
        let kept = cat.list_peer_cache(tid, 20).unwrap();
        assert!(kept.contains(&m1), "manual m1 pruned: {kept:?}");
        assert!(kept.contains(&m2), "manual m2 pruned: {kept:?}");
    }

    #[test]
    fn schema_v7_indexes_exist() {
        let cat = Catalog::open_in_memory().unwrap();
        let mut stmt = cat
            .conn
            .prepare("SELECT name FROM sqlite_master WHERE type='index' AND name LIKE 'idx_%'")
            .unwrap();
        let names: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert!(
            names.iter().any(|n| n == "idx_peer_cache_torrent_seen"),
            "missing peer_cache index: {names:?}"
        );
        assert!(
            names.iter().any(|n| n == "idx_tracker_torrent_url"),
            "missing tracker index: {names:?}"
        );
        assert!(
            names.iter().any(|n| n == "idx_torrent_want_start_deleted"),
            "missing want_start index: {names:?}"
        );
    }
}

#[cfg(test)]
mod soft_delete_purge_tests {
    use super::*;

    use rusqlite::params;

    use crate::catalog::{Catalog, TorrentInsert};

    fn insert_named(cat: &mut Catalog, name: &str, ih_last: u8) -> i64 {
        let mut ih = [0u8; 20];
        ih[19] = ih_last;
        cat.conn
            .execute(
                "INSERT INTO torrent (infohash, name, total_size, piece_length, piece_count, private, created_at)
                 VALUES (?1, ?2, 1, 16384, 1, 0, 0)",
                params![&ih[..], name],
            )
            .unwrap();
        cat.conn.last_insert_rowid()
    }

    #[test]
    fn purge_removes_old_soft_deleted_catalog_only() {
        let mut cat = Catalog::open_in_memory().unwrap();
        let old = insert_named(&mut cat, "old", 1);
        let recent = insert_named(&mut cat, "recent", 2);
        let active = insert_named(&mut cat, "active", 3);

        cat.mark_deleted(old).unwrap();
        cat.mark_deleted(recent).unwrap();

        // Age "old" beyond 30 days; leave "recent" fresh.
        let aged = unix_now_secs() - 40 * 86_400;
        cat.conn
            .execute(
                "UPDATE torrent SET deleted_at = ?1 WHERE id = ?2",
                params![aged, old],
            )
            .unwrap();

        assert_eq!(cat.purge_soft_deleted(0).unwrap(), 0); // disabled
        assert_eq!(cat.purge_soft_deleted(30).unwrap(), 1);

        assert!(
            cat.conn
                .query_row(
                    "SELECT COUNT(*) FROM torrent WHERE id = ?1",
                    params![old],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap()
                == 0
        );
        assert!(cat.is_deleted(recent).unwrap());
        assert!(!cat.is_deleted(active).unwrap());
        // Active + recent still present.
        let n: i64 = cat
            .conn
            .query_row("SELECT COUNT(*) FROM torrent", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2);
    }

    #[test]
    fn restore_clears_deleted_at() {
        let mut cat = Catalog::open_in_memory().unwrap();
        let id = insert_named(&mut cat, "t", 9);
        cat.mark_deleted(id).unwrap();
        let at: Option<i64> = cat
            .conn
            .query_row(
                "SELECT deleted_at FROM torrent WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(at.is_some());

        let meta = crate::metainfo::Metainfo {
            infohash: {
                let mut h = [0u8; 20];
                h[19] = 9;
                h
            },
            name: "t".into(),
            piece_length: 16384,
            piece_count: 1,
            total_size: 1,
            pieces: vec![0u8; 20],
            files: vec![],
            is_multi_file: false,
            private: false,
            trackers: vec![],
            announce: None,
        };
        let ins = TorrentInsert::from_metainfo(meta, "/tmp");
        cat.restore_deleted(id, &ins).unwrap();
        let at: Option<i64> = cat
            .conn
            .query_row(
                "SELECT deleted_at FROM torrent WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(at.is_none());
        assert!(!cat.is_deleted(id).unwrap());
    }
}

#[cfg(test)]
mod storage_audit_tests {
    use std::io::Write;

    use rusqlite::params;

    use crate::catalog::{Catalog, TorrentInsert};

    fn insert_complete_with_file(
        cat: &mut Catalog,
        dir: &std::path::Path,
        name: &str,
        ih_last: u8,
        size: u64,
        payload: Option<&[u8]>,
    ) -> i64 {
        let mut ih = [0u8; 20];
        ih[19] = ih_last;
        let pieces = vec![0u8; 20];
        let meta = crate::metainfo::Metainfo {
            infohash: ih,
            name: name.into(),
            piece_length: 16384,
            piece_count: 1,
            total_size: size,
            pieces,
            files: vec![crate::metainfo::TorrentFile {
                path: std::path::PathBuf::from(format!("{name}.bin")),
                size,
                offset: 0,
            }],
            is_multi_file: false,
            private: false,
            trackers: vec![],
            announce: None,
        };
        let mut ins = TorrentInsert::from_metainfo(meta, dir.display().to_string());
        ins.complete = true;
        ins.have_count = 1;
        ins.want_start = true;
        ins.state = "started".into();
        let id = cat.insert_torrent(&ins).unwrap().id();
        // insert_torrent may not mark complete if from_metainfo defaults — force.
        cat.conn
            .execute(
                "UPDATE torrent SET complete = 1, want_start = 1, state = 'started' WHERE id = ?1",
                params![id],
            )
            .unwrap();
        cat.conn
            .execute(
                "UPDATE bitfield SET bits = NULL, have_count = 1 WHERE torrent_id = ?1",
                params![id],
            )
            .unwrap();
        if let Some(bytes) = payload {
            let path = dir.join(format!("{name}.bin"));
            let mut f = std::fs::File::create(&path).unwrap();
            f.write_all(bytes).unwrap();
        }
        id
    }

    #[test]
    fn audit_ok_leaves_complete() {
        let dir = tempfile::tempdir().unwrap();
        let mut cat = Catalog::open_in_memory().unwrap();
        let payload = vec![7u8; 32];
        let id = insert_complete_with_file(&mut cat, dir.path(), "ok", 1, 32, Some(&payload));
        let rep = cat.audit_complete_storage().unwrap();
        assert_eq!(rep.checked, 1);
        assert_eq!(rep.demoted, 0);
        let (complete, _, have) = cat.load_bitfield_bytes(id).unwrap();
        assert!(complete);
        assert_eq!(have, 1);
        let want: i64 = cat
            .conn
            .query_row(
                "SELECT want_start FROM torrent WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(want, 1);
    }

    #[test]
    fn audit_missing_demotes_and_stops() {
        let dir = tempfile::tempdir().unwrap();
        let mut cat = Catalog::open_in_memory().unwrap();
        let id = insert_complete_with_file(&mut cat, dir.path(), "gone", 2, 32, None);
        let rep = cat.audit_complete_storage().unwrap();
        assert_eq!(rep.demoted, 1);
        let (complete, _, have) = cat.load_bitfield_bytes(id).unwrap();
        assert!(!complete);
        assert_eq!(have, 0);
        let (want, state, err): (i64, String, Option<String>) = cat
            .conn
            .query_row(
                "SELECT want_start, state, error_msg FROM torrent WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(want, 0);
        assert_eq!(state, "stopped");
        assert!(err.as_deref().unwrap_or("").contains("missing"));
        assert!(!dir.path().join("gone.bin").exists());
        let row = cat
            .list_torrents()
            .unwrap()
            .into_iter()
            .find(|r| r.id == id)
            .unwrap();
        assert!(!row.want_start);
        assert!(row.error_msg.as_ref().unwrap().contains("missing"));
    }

    #[test]
    fn audit_long_file_demotes() {
        let dir = tempfile::tempdir().unwrap();
        let mut cat = Catalog::open_in_memory().unwrap();
        let payload = vec![1u8; 64]; // longer than size 32
        let id = insert_complete_with_file(&mut cat, dir.path(), "long", 3, 32, Some(&payload));
        let rep = cat.audit_complete_storage().unwrap();
        assert_eq!(rep.demoted, 1);
        let (complete, _, _) = cat.load_bitfield_bytes(id).unwrap();
        assert!(!complete);
        let err: String = cat
            .conn
            .query_row(
                "SELECT error_msg FROM torrent WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(err.contains("size mismatch"));
    }

    #[test]
    fn audit_short_file_demotes() {
        let dir = tempfile::tempdir().unwrap();
        let mut cat = Catalog::open_in_memory().unwrap();
        let payload = vec![1u8; 8];
        let id = insert_complete_with_file(&mut cat, dir.path(), "short", 4, 32, Some(&payload));
        assert_eq!(cat.audit_complete_storage().unwrap().demoted, 1);
        let (complete, _, have) = cat.load_bitfield_bytes(id).unwrap();
        assert!(!complete && have == 0);
    }

    #[test]
    fn audit_skips_soft_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let mut cat = Catalog::open_in_memory().unwrap();
        let id = insert_complete_with_file(&mut cat, dir.path(), "del", 5, 8, None);
        cat.set_want_start(id, false).unwrap();
        cat.mark_deleted(id).unwrap();
        // still complete=1 but deleted
        cat.conn
            .execute("UPDATE torrent SET complete = 1 WHERE id = ?1", params![id])
            .unwrap();
        let rep = cat.audit_complete_storage().unwrap();
        assert_eq!(rep.checked, 0);
        assert_eq!(rep.demoted, 0);
    }
}
