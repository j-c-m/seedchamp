use super::*;
use crate::catalog::{all_set_bitfield, bitfield_get, empty_bitfield};
use crate::disk::{FileLayout, StorageLayout};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// Two files, one piece each (piece_length = file size).
fn two_file_layout() -> StorageLayout {
    StorageLayout {
        data_root: PathBuf::from("/tmp"),
        piece_length: 32,
        piece_count: 2,
        total_size: 64,
        files: vec![
            FileLayout {
                path: PathBuf::from("want"),
                size: 32,
                offset: 0,
                priority: 1,
            },
            FileLayout {
                path: PathBuf::from("off"),
                size: 32,
                offset: 32,
                priority: 0,
            },
        ],
    }
}

#[test]
fn download_complete_with_off_files_missing() {
    let layout = two_file_layout();
    let hashes = vec![0u8; 40];
    let t = HotTorrent::new_empty(1, [0u8; 20], "t".into(), layout, hashes);
    assert!(!t.is_download_complete());
    assert_eq!(t.missing_piece_count(), 1); // only wanted piece
                                            // left = full torrent remaining, not wanted-only.
    assert_eq!(t.left_bytes(), 64);

    // Get wanted piece only — off file still missing.
    assert!(t.wants_piece(0));
    assert!(!t.wants_piece(1));
    t.mark_have(0);
    assert!(t.is_download_complete());
    assert!(!t.is_complete());
    assert_eq!(t.missing_piece_count(), 0);
    assert_eq!(t.left_bytes(), 32); // off piece still counts as left
                                    // Full torrent still incomplete until off piece arrives.
    t.mark_have(1);
    assert!(t.is_complete());
    assert!(t.is_download_complete());
    assert_eq!(t.left_bytes(), 0);
}

#[test]
fn mark_have_notifies_subscribers_once() {
    let layout = two_file_layout();
    let hashes = vec![0u8; 40];
    let t = HotTorrent::new_empty(1, [0u8; 20], "t".into(), layout, hashes);
    let rx = t.subscribe_have();
    assert_eq!(t.have_hub.subscriber_count(), 1);
    t.mark_have(0);
    assert_eq!(rx.try_recv().ok(), Some(0));
    // Already have — no second notify.
    t.mark_have(0);
    assert!(rx.try_recv().is_err());
    // Drop subscriber; publish prunes dead senders.
    drop(rx);
    t.mark_have(1);
    assert_eq!(t.have_hub.subscriber_count(), 0);
}

#[test]
fn piece_claim_exclusive_until_endgame() {
    let layout = two_file_layout();
    let hashes = vec![0u8; 40];
    let t = HotTorrent::new_empty(1, [0u8; 20], "t".into(), layout, hashes);
    assert!(t.try_claim_piece(0, false));
    assert!(
        !t.try_claim_piece(0, false),
        "second peer blocked outside endgame"
    );
    assert!(t.try_claim_piece(0, true), "endgame allows multi-source");
    t.release_piece_claim(0);
    assert!(t.try_claim_piece(0, false));
    t.mark_have(0);
    assert!(
        !t.try_claim_piece(0, false),
        "have pieces are not claimable"
    );
    assert!(
        !t.try_claim_piece(0, true),
        "have pieces not claimable in endgame either"
    );
}

#[test]
fn pick_endgame_prefers_already_racing_piece() {
    // Piece 0 rarer but free; piece 1 common and already in_flight.
    // Steady rarest → 0; endgame → join race on 1.
    let layout = two_file_layout();
    let mut layout = layout;
    layout.files[1].priority = 1;
    let hashes = vec![0u8; 40];
    let t = HotTorrent::new_empty(1, [0u8; 20], "t".into(), layout, hashes);
    t.avail_inc(0); // rarer
    t.avail_inc(1);
    t.avail_inc(1);
    t.avail_inc(1); // common
    let mut peer_bf = empty_bitfield(2);
    crate::catalog::bitfield_set(&mut peer_bf, 0);
    crate::catalog::bitfield_set(&mut peer_bf, 1);
    assert!(t.try_claim_piece(1, true)); // race already started on common piece
    let mut claimed = HashSet::new();
    let pick = t.pick_rarest_piece(
        &peer_bf,
        |_| false,
        |_| false,
        |i| claimed.insert(i),
        |_| true,
        true, // endgame
    );
    assert_eq!(
        pick.map(|p| p.0),
        Some(1),
        "endgame should join in_flight race even if piece is more common"
    );
}

#[test]
fn pick_prefers_rarer_piece() {
    let layout = two_file_layout();
    let mut layout = layout;
    layout.files[1].priority = 1;
    let hashes = vec![0u8; 40];
    let t = HotTorrent::new_empty(1, [0u8; 20], "t".into(), layout, hashes);
    // Piece 0 common (3 peers), piece 1 rare (1 peer).
    t.avail_inc(0);
    t.avail_inc(0);
    t.avail_inc(0);
    t.avail_inc(1);
    let mut peer_bf = empty_bitfield(2);
    crate::catalog::bitfield_set(&mut peer_bf, 0);
    crate::catalog::bitfield_set(&mut peer_bf, 1);
    let mut claimed = HashSet::new();
    let pick = t.pick_rarest_piece(
        &peer_bf,
        |_| false,
        |_| false,
        |i| claimed.insert(i),
        |_| true,
        false,
    );
    assert_eq!(pick.map(|p| p.0), Some(1), "should prefer rarer piece");
}

#[test]
fn pick_falls_back_to_common_when_rare_claimed() {
    // Soft rarest: rarer is claimed elsewhere → still take any missing piece.
    let layout = two_file_layout();
    let mut layout = layout;
    layout.files[1].priority = 1;
    let hashes = vec![0u8; 40];
    let t = HotTorrent::new_empty(1, [0u8; 20], "t".into(), layout, hashes);
    t.avail_inc(0);
    t.avail_inc(0);
    t.avail_inc(0);
    t.avail_inc(1); // rarer
    let mut peer_bf = empty_bitfield(2);
    crate::catalog::bitfield_set(&mut peer_bf, 0);
    crate::catalog::bitfield_set(&mut peer_bf, 1);
    // Rarer piece already exclusive-claimed.
    assert!(t.try_claim_piece(1, false));
    let pick = t.pick_rarest_piece(
        &peer_bf,
        |_| false,
        |_| false,
        |i| t.try_claim_piece(i, false),
        |_| true,
        false,
    );
    assert_eq!(
        pick.map(|p| p.0),
        Some(0),
        "fallback to common missing piece when rare is claimed"
    );
}

#[test]
fn pick_rarest_none_when_peer_has_nothing() {
    let layout = two_file_layout();
    let mut layout = layout;
    layout.files[1].priority = 1;
    let hashes = vec![0u8; 40];
    let t = HotTorrent::new_empty(1, [0u8; 20], "t".into(), layout, hashes);
    let peer_bf = empty_bitfield(2);
    let pick = t.pick_rarest_piece(&peer_bf, |_| false, |_| false, |_| true, |_| true, false);
    assert!(pick.is_none());
}

/// Last 1–2 pieces of a large torrent must always be pickable (exact scan).
/// Bounded random sample alone misses ~30–60% of the time and hung sc-sc at 1022/1024.
#[test]
fn pick_rarest_finds_last_pieces_of_large_torrent() {
    const PC: u32 = 1024;
    let plen = 32u32;
    let layout = StorageLayout {
        data_root: PathBuf::from("/tmp"),
        piece_length: plen,
        piece_count: PC,
        total_size: PC as u64 * plen as u64,
        files: vec![FileLayout {
            path: PathBuf::from("big"),
            size: PC as u64 * plen as u64,
            offset: 0,
            priority: 1,
        }],
    };
    let hashes = vec![0u8; PC as usize * 20];
    let t = HotTorrent::new_empty(1, [0u8; 20], "big".into(), layout, hashes);
    // Leave only pieces 17 and 900 missing (spread out so a short walk misses).
    for i in 0..PC {
        if i != 17 && i != 900 {
            t.mark_have(i);
        }
    }
    assert_eq!(t.missing_piece_count(), 2);
    let peer_bf = all_set_bitfield(PC);
    for _ in 0..50 {
        let mut claimed = HashSet::new();
        let a = t
            .pick_rarest_piece(
                &peer_bf,
                |_| false,
                |_| false,
                |i| claimed.insert(i),
                |_| true,
                false,
            )
            .expect("first of last-two")
            .0;
        assert!(a == 17 || a == 900, "picked unexpected {a}");
        let b = t
            .pick_rarest_piece(
                &peer_bf,
                |i| i == a,
                |_| false,
                |i| claimed.insert(i),
                |_| true,
                false,
            )
            .expect("second of last-two")
            .0;
        assert_ne!(a, b);
        assert!(b == 17 || b == 900, "picked unexpected {b}");
    }
    // Single last piece after one have.
    t.mark_have(17);
    for _ in 0..50 {
        let pick = t
            .pick_rarest_piece(&peer_bf, |_| false, |_| false, |_| true, |_| true, false)
            .expect("last piece");
        assert_eq!(pick.0, 900);
    }
}

#[test]
fn pick_rarest_skips_failed_claims() {
    let layout = two_file_layout();
    let mut layout = layout;
    layout.files[1].priority = 1;
    let hashes = vec![0u8; 40];
    let t = HotTorrent::new_empty(1, [0u8; 20], "t".into(), layout, hashes);
    // Equal rarity; claim always fails for 0.
    let mut peer_bf = empty_bitfield(2);
    crate::catalog::bitfield_set(&mut peer_bf, 0);
    crate::catalog::bitfield_set(&mut peer_bf, 1);
    let pick = t.pick_rarest_piece(&peer_bf, |_| false, |_| false, |i| i != 0, |_| true, false);
    assert_eq!(pick.map(|p| p.0), Some(1));
}

#[test]
fn pick_rarest_respects_piece_ok_filter() {
    // B3: while choked, only Allowed Fast pieces.
    let layout = two_file_layout();
    let mut layout = layout;
    layout.files[1].priority = 1;
    let hashes = vec![0u8; 40];
    let t = HotTorrent::new_empty(1, [0u8; 20], "t".into(), layout, hashes);
    t.avail_inc(0);
    t.avail_inc(0);
    t.avail_inc(1); // rarer
    let mut peer_bf = empty_bitfield(2);
    crate::catalog::bitfield_set(&mut peer_bf, 0);
    crate::catalog::bitfield_set(&mut peer_bf, 1);
    let mut claimed = HashSet::new();
    // Only piece 0 allowed (even though 1 is rarer).
    let pick = t.pick_rarest_piece(
        &peer_bf,
        |_| false,
        |_| false,
        |i| claimed.insert(i),
        |i| i == 0,
        false,
    );
    assert_eq!(pick.map(|p| p.0), Some(0));
}

/// Concurrent pick + mark_have must not deadlock on parking_lot fair RwLock.
///
/// Regression: `pick_rarest_piece` held `pieces.read()` then re-entered via
/// `missing_piece_count()`; a waiting `mark_have` writer blocked the nested
/// read forever and froze peer I/O.
///
/// Also stresses rebuild / has_piece / bitfield_snapshot / claim — the same
/// hot locks hit while seed-while-leech is verifying pieces.
#[test]
fn pick_rarest_and_mark_have_no_deadlock() {
    use std::sync::atomic::AtomicBool;
    use std::thread;
    use std::time::{Duration, Instant};

    let layout = StorageLayout {
        data_root: PathBuf::from("/tmp"),
        piece_length: 32,
        piece_count: 64,
        total_size: 64 * 32,
        files: vec![FileLayout {
            path: PathBuf::from("f"),
            size: 64 * 32,
            offset: 0,
            priority: 1,
        }],
    };
    let hashes = vec![0u8; 64 * 20];
    let t = Arc::new(HotTorrent::new_empty(
        1,
        [0u8; 20],
        "t".into(),
        layout,
        hashes,
    ));
    let mut peer_bf = empty_bitfield(64);
    for i in 0..64 {
        crate::catalog::bitfield_set(&mut peer_bf, i);
    }
    let peer_bf = Arc::new(peer_bf);
    let stop = Arc::new(AtomicBool::new(false));
    let mut joins = Vec::new();

    for _ in 0..4 {
        let t = Arc::clone(&t);
        let peer_bf = Arc::clone(&peer_bf);
        let stop = Arc::clone(&stop);
        joins.push(thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let _ = t.pick_rarest_piece(
                    peer_bf.as_slice(),
                    |_| false,
                    |_| false,
                    |i| t.try_claim_piece(i, true),
                    |_| true,
                    true,
                );
                // Upload path + TUI-style reads under concurrent mark_have.
                let _ = t.has_piece(0);
                let _ = t.bitfield_snapshot();
                let _ = t.have_count();
                let _ = t.missing_piece_count();
                let _ = t.next_interest_piece(&|i| bitfield_get(peer_bf.as_slice(), i));
            }
        }));
    }
    for _ in 0..2 {
        let t = Arc::clone(&t);
        let stop = Arc::clone(&stop);
        joins.push(thread::spawn(move || {
            let mut i = 0u32;
            while !stop.load(Ordering::Relaxed) {
                t.mark_have(i % 64);
                i = i.wrapping_add(1);
                // Also hit claim/release paths that take in_flight write.
                let _ = t.try_claim_piece(i % 64, false);
                t.release_piece_claim(i % 64);
                // Priority rebuild nests wanted/pieces — must not deadlock with pick.
                if i.is_multiple_of(17) {
                    t.rebuild_wanted_and_missing();
                }
            }
        }));
    }

    // Old bug hung forever here under concurrent pick+have.
    thread::sleep(Duration::from_millis(300));
    stop.store(true, Ordering::Relaxed);
    let deadline = Instant::now() + Duration::from_secs(5);
    for j in joins {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            panic!("pick_rarest/mark_have deadlock: join timed out");
        }
        // join has no timeout; park a watcher thread that panics if late.
        let (tx, rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let _ = j.join();
            let _ = tx.send(());
        });
        if rx.recv_timeout(left).is_err() {
            panic!("pick_rarest/mark_have deadlock: worker did not exit");
        }
    }
}

#[test]
fn should_endgame_when_all_remaining_claimed() {
    let layout = two_file_layout();
    let hashes = vec![0u8; 40];
    // Both files wanted.
    let mut layout = layout;
    layout.files[1].priority = 1;
    let t = HotTorrent::new_empty(1, [0u8; 20], "t".into(), layout, hashes);
    assert_eq!(t.missing_piece_count(), 2);
    // 2 missing ≤ ENDGAME_MAX_MISSING → endgame (not gated on seed count).
    assert!(t.should_endgame());
    t.mark_have(0);
    assert!(t.should_endgame()); // 1 left
    t.mark_have(1);
    assert!(!t.should_endgame()); // complete
}

#[test]
fn turning_file_off_stops_leech_need() {
    let mut layout = two_file_layout();
    layout.files[1].priority = 1; // both wanted at start
    let hashes = vec![0u8; 40];
    let t = HotTorrent::new_empty(1, [0u8; 20], "t".into(), layout, hashes);
    assert_eq!(t.missing_piece_count(), 2);
    t.mark_have(0);
    assert!(!t.is_download_complete());
    // User marks second file off — no more dials needed.
    t.set_file_priority(1, 0);
    assert!(t.is_download_complete());
    // left stays full-torrent remaining (piece 1 still missing).
    assert_eq!(t.left_bytes(), 32);
}

#[test]
fn turning_file_on_reopens_leech_need() {
    let layout = two_file_layout();
    let hashes = vec![0u8; 40];
    let t = HotTorrent::new_empty(1, [0u8; 20], "t".into(), layout, hashes);
    t.mark_have(0);
    assert!(t.is_download_complete());
    assert_eq!(t.left_bytes(), 32);
    t.set_file_priority(1, 1);
    assert!(!t.is_download_complete());
    assert_eq!(t.missing_piece_count(), 1);
    // Priority change does not alter full-torrent left.
    assert_eq!(t.left_bytes(), 32);
}

#[test]
fn mark_have_releases_staging_pool() {
    let layout = two_file_layout();
    let hashes = vec![0u8; 40];
    let t = HotTorrent::new_empty(1, [0u8; 20], "t".into(), layout, hashes);
    t.ensure_staging_pool();
    assert!(t.staging_pool().is_some());
    t.mark_have(0);
    assert!(t.is_download_complete());
    assert!(t.staging_pool().is_none());
}

#[test]
fn file_priority_releases_and_rebinds_staging() {
    let mut layout = two_file_layout();
    layout.files[1].priority = 1;
    let hashes = vec![0u8; 40];
    let t = HotTorrent::new_empty(1, [0u8; 20], "t".into(), layout, hashes);
    t.ensure_staging_pool();
    t.mark_have(0);
    t.set_file_priority(1, 0);
    assert!(t.is_download_complete());
    assert!(t.staging_pool().is_none());
    t.set_file_priority(1, 1);
    assert!(!t.is_download_complete());
    assert!(t.staging_pool().is_some());
}
