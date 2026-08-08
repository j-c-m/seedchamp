use std::thread;
use std::time::Duration;

use sha1::{Digest, Sha1};
use std::io::Write;

use crate::catalog::{Catalog, TorrentInsert};
use crate::metainfo::Metainfo;
use crate::session::RuntimeConfig;

use super::{spawn_control_plane, ControlEvent};

fn make_torrent_bytes(payload: &[u8], piece_length: u32) -> Vec<u8> {
    let mut pieces = Vec::new();
    let mut off = 0usize;
    while off < payload.len() {
        let end = (off + piece_length as usize).min(payload.len());
        let mut h = Sha1::new();
        h.update(&payload[off..end]);
        pieces.extend_from_slice(&h.finalize());
        off = end;
    }
    let name = b"data.bin";
    let mut info = Vec::new();
    info.extend_from_slice(format!("d6:lengthi{}e", payload.len()).as_bytes());
    info.extend_from_slice(b"4:name");
    info.extend_from_slice(format!("{}:", name.len()).as_bytes());
    info.extend_from_slice(name);
    info.extend_from_slice(format!("12:piece lengthi{piece_length}e").as_bytes());
    info.extend_from_slice(format!("6:pieces{}:", pieces.len()).as_bytes());
    info.extend_from_slice(&pieces);
    info.extend_from_slice(b"e");
    let mut root = Vec::new();
    root.extend_from_slice(b"d8:announce8:http://x4:info");
    root.extend_from_slice(&info);
    root.extend_from_slice(b"e");
    root
}

#[test]
fn control_start_stop_persists_want_start_and_hot_set() {
    let dir = tempfile::tempdir().unwrap();
    let payload: Vec<u8> = (0u8..200).collect();
    let torrent_bytes = make_torrent_bytes(&payload, 64);
    let m = Metainfo::parse_bytes(&torrent_bytes).unwrap();
    let data_path = dir.path().join("data.bin");
    std::fs::File::create(&data_path)
        .unwrap()
        .write_all(&payload)
        .unwrap();

    let db = dir.path().join("c.sqlite");
    let mut cat = Catalog::open(&db).unwrap();
    let mut ins = TorrentInsert::from_metainfo(m, dir.path().display().to_string());
    ins.want_start = false;
    let id = cat.insert_torrent(&ins).unwrap().id();
    drop(cat);

    // Ephemeral listen port so tests don't collide.
    let cfg = RuntimeConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        announce: false,
        peer_workers: Some(2),
        ..RuntimeConfig::default()
    };
    let (handle, _plane) = spawn_control_plane(&db, cfg).unwrap();

    let mut ready = false;
    for _ in 0..200 {
        for e in handle.drain_events() {
            if matches!(e, ControlEvent::Ready { .. }) {
                ready = true;
            }
        }
        if ready {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(ready, "control never Ready");

    handle.request_start(id).unwrap();
    let mut started = false;
    let mut fail = None;
    for _ in 0..300 {
        for e in handle.drain_events() {
            match e {
                ControlEvent::Started { id: i } if i == id => started = true,
                ControlEvent::StartFailed { id: i, error } if i == id => fail = Some(error),
                _ => {}
            }
        }
        if started || fail.is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(fail.is_none(), "start failed: {fail:?}");
    assert!(started, "never received Started event");

    let cat = Catalog::open(&db).unwrap();
    let row = cat
        .list_torrents()
        .unwrap()
        .into_iter()
        .find(|r| r.id == id)
        .unwrap();
    assert!(
        row.want_start,
        "catalog want_start must be true after start"
    );
    drop(cat);

    let snap = handle.snapshot().unwrap();
    assert!(
        snap.torrents.iter().any(|t| t.id == id),
        "hot set missing torrent after start: {:?}",
        snap.torrents
    );

    handle.request_stop(id).unwrap();
    let mut stopped = false;
    let mut stop_fail = None;
    let mut seen = Vec::new();
    for _ in 0..500 {
        for e in handle.drain_events() {
            seen.push(format!("{e:?}"));
            match &e {
                ControlEvent::Stopped { id: i } if *i == id => stopped = true,
                ControlEvent::StopFailed { id: i, error } if *i == id => {
                    stop_fail = Some(error.clone());
                }
                _ => {}
            }
        }
        if stopped || stop_fail.is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        stop_fail.is_none(),
        "stop failed: {stop_fail:?}; events={seen:?}"
    );
    assert!(stopped, "never received Stopped event; events={seen:?}");

    let cat = Catalog::open(&db).unwrap();
    let row = cat
        .list_torrents()
        .unwrap()
        .into_iter()
        .find(|r| r.id == id)
        .unwrap();
    assert!(
        !row.want_start,
        "catalog want_start must be false after stop"
    );

    let snap = handle.snapshot().unwrap();
    assert!(
        !snap.torrents.iter().any(|t| t.id == id),
        "hot set still has torrent after stop"
    );

    handle.shutdown();
}

fn wait_ready(handle: &super::ControlHandle) {
    for _ in 0..200 {
        if handle
            .drain_events()
            .iter()
            .any(|e| matches!(e, ControlEvent::Ready { .. }))
        {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("control plane never ready");
}

fn wait_started(handle: &super::ControlHandle, id: i64) {
    for _ in 0..300 {
        if handle
            .drain_events()
            .iter()
            .any(|e| matches!(e, ControlEvent::Started { id: i } if *i == id))
        {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("never Started #{id}");
}

fn wait_stopped(handle: &super::ControlHandle, id: i64) {
    for _ in 0..300 {
        if handle
            .drain_events()
            .iter()
            .any(|e| matches!(e, ControlEvent::Stopped { id: i } if *i == id))
        {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("never Stopped #{id}");
}

fn setup_control_with_torrent() -> (
    tempfile::TempDir,
    i64,
    super::ControlHandle,
    super::ControlPlane,
) {
    let dir = tempfile::tempdir().unwrap();
    let payload: Vec<u8> = (0u8..200).collect();
    let torrent_bytes = make_torrent_bytes(&payload, 64);
    let m = Metainfo::parse_bytes(&torrent_bytes).unwrap();
    let data_path = dir.path().join("data.bin");
    std::fs::File::create(&data_path)
        .unwrap()
        .write_all(&payload)
        .unwrap();

    let db = dir.path().join("c.sqlite");
    let mut cat = Catalog::open(&db).unwrap();
    let mut ins = TorrentInsert::from_metainfo(m, dir.path().display().to_string());
    ins.want_start = false;
    let id = cat.insert_torrent(&ins).unwrap().id();
    drop(cat);

    let cfg = RuntimeConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        announce: false,
        peer_workers: Some(2),
        ..RuntimeConfig::default()
    };
    let (handle, plane) = spawn_control_plane(&db, cfg).unwrap();
    wait_ready(&handle);
    (dir, id, handle, plane)
}

#[test]
fn control_soft_delete_rejects_started() {
    let (_dir, id, handle, _plane) = setup_control_with_torrent();

    handle.request_start(id).unwrap();
    wait_started(&handle, id);

    handle.request_soft_delete(id).unwrap();
    let mut fail = None;
    for _ in 0..200 {
        for e in handle.drain_events() {
            match e {
                ControlEvent::SoftDeleted { id: i } if i == id => {
                    panic!("soft-delete must not succeed while started");
                }
                ControlEvent::SoftDeleteFailed { id: i, error } if i == id => {
                    fail = Some(error);
                }
                _ => {}
            }
        }
        if fail.is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let err = fail.expect("expected SoftDeleteFailed while started");
    assert!(err.contains("started"), "unexpected error: {err}");

    let snap = handle.snapshot().unwrap();
    assert!(
        snap.torrents.iter().any(|t| t.id == id),
        "started torrent must stay hot after rejected delete"
    );

    handle.shutdown();
}

#[test]
fn control_soft_delete_after_stop_hides_from_list() {
    let (dir, id, handle, _plane) = setup_control_with_torrent();
    let db = dir.path().join("c.sqlite");

    handle.request_start(id).unwrap();
    wait_started(&handle, id);
    handle.request_stop(id).unwrap();
    wait_stopped(&handle, id);

    handle.request_soft_delete(id).unwrap();
    let mut deleted = false;
    let mut fail = None;
    for _ in 0..500 {
        for e in handle.drain_events() {
            match e {
                ControlEvent::SoftDeleted { id: i } if i == id => deleted = true,
                ControlEvent::SoftDeleteFailed { id: i, error } if i == id => {
                    fail = Some(error);
                }
                _ => {}
            }
        }
        if deleted || fail.is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(fail.is_none(), "soft-delete failed: {fail:?}");
    assert!(deleted, "never received SoftDeleted");

    let cat = Catalog::open(&db).unwrap();
    assert!(
        cat.list_torrents().unwrap().is_empty(),
        "soft-deleted torrent must not appear in list"
    );
    assert!(cat.is_deleted(id).unwrap());
    drop(cat);

    handle.shutdown();
}

#[test]
fn control_remove_rejects_started() {
    let (_dir, id, handle, _plane) = setup_control_with_torrent();

    handle.request_start(id).unwrap();
    wait_started(&handle, id);

    handle.request_remove(id).unwrap();
    let mut fail = None;
    for _ in 0..200 {
        for e in handle.drain_events() {
            match e {
                ControlEvent::Removed { id: i } if i == id => {
                    panic!("remove must not succeed while started");
                }
                ControlEvent::RemoveFailed { id: i, error } if i == id => {
                    fail = Some(error);
                }
                _ => {}
            }
        }
        if fail.is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let err = fail.expect("expected RemoveFailed while started");
    assert!(err.contains("started"), "unexpected error: {err}");

    handle.shutdown();
}
