//! Disk worker unit tests.

use super::*;
use crate::disk::spans::FileLayout;
use crate::disk::{ensure_storage, read_piece, FdCache};
use crate::runtime::HashOutcome;
use flume;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn test_layout(dir: &std::path::Path) -> Arc<StorageLayout> {
    Arc::new(StorageLayout {
        data_root: dir.to_path_buf(),
        piece_length: 32,
        piece_count: 1,
        total_size: 32,
        files: vec![FileLayout {
            path: PathBuf::from("f.bin"),
            size: 32,
            offset: 0,
            priority: 1,
        }],
    })
}

fn write_roundtrip(backend: &str) {
    let dir = tempfile::tempdir().unwrap();
    let layout = test_layout(dir.path());
    ensure_storage(&layout).unwrap();
    let w = DiskWorker::spawn_with_options(false, backend, DEFAULT_DISK_DEPTH).unwrap();
    if backend == "thread" {
        assert_eq!(w.backend_kind(), DiskBackendKind::Thread);
    } else if backend == "uring" {
        assert_eq!(w.backend_kind(), DiskBackendKind::Uring);
    } else if backend == "aio" {
        assert_eq!(w.backend_kind(), DiskBackendKind::Aio);
    }
    let (tx, rx) = flume::unbounded();
    let data: Vec<u8> = (0..32).collect();
    if let Err((e, _)) = w.submit_write(DiskWriteJob {
        index: 0,
        plen: 32,
        data: data.clone(),
        layout: Arc::clone(&layout),
        reply: tx,
    }) {
        panic!("submit: {e}");
    }
    let outcome = rx.recv().expect("outcome");
    match outcome {
        HashOutcome::Ok { data: d, .. } => assert_eq!(d, data),
        HashOutcome::HashFail { .. } | HashOutcome::WriteFail { .. } => {
            panic!("write failed on backend {backend}")
        }
    }
    let mut cache = FdCache::default_cache();
    let mut out = Vec::new();
    read_piece(&mut cache, &layout, 0, &mut out).unwrap();
    assert_eq!(out, data);
}

fn multi_span_layout(dir: &std::path::Path) -> Arc<StorageLayout> {
    // Piece 0 = 16 bytes in a.bin + 16 bytes in b.bin.
    Arc::new(StorageLayout {
        data_root: dir.to_path_buf(),
        piece_length: 32,
        piece_count: 1,
        total_size: 32,
        files: vec![
            FileLayout {
                path: PathBuf::from("a.bin"),
                size: 16,
                offset: 0,
                priority: 1,
            },
            FileLayout {
                path: PathBuf::from("b.bin"),
                size: 16,
                offset: 16,
                priority: 1,
            },
        ],
    })
}

fn write_roundtrip_multi_span(backend: &str) {
    let dir = tempfile::tempdir().unwrap();
    let layout = multi_span_layout(dir.path());
    ensure_storage(&layout).unwrap();
    let w = DiskWorker::spawn_with_options(false, backend, DEFAULT_DISK_DEPTH).unwrap();
    let (tx, rx) = flume::unbounded();
    let data: Vec<u8> = (0..32).map(|i| i as u8).collect();
    if let Err((e, _)) = w.submit_write(DiskWriteJob {
        index: 0,
        plen: 32,
        data: data.clone(),
        layout: Arc::clone(&layout),
        reply: tx,
    }) {
        panic!("submit: {e}");
    }
    match rx.recv().expect("outcome") {
        HashOutcome::Ok { data: d, .. } => assert_eq!(d, data),
        HashOutcome::HashFail { .. } | HashOutcome::WriteFail { .. } => {
            panic!("multi-span write failed on {backend}")
        }
    }
    let mut cache = FdCache::default_cache();
    let mut out = Vec::new();
    read_piece(&mut cache, &layout, 0, &mut out).unwrap();
    assert_eq!(out, data);
}

#[test]
fn thread_backend_write_roundtrip() {
    write_roundtrip("thread");
}

#[test]
fn thread_backend_multi_span_roundtrip() {
    write_roundtrip_multi_span("thread");
}

#[cfg(target_os = "linux")]
#[test]
fn uring_backend_write_roundtrip() {
    write_roundtrip("uring");
}

#[cfg(target_os = "linux")]
#[test]
fn uring_backend_multi_span_roundtrip() {
    write_roundtrip_multi_span("uring");
}

#[cfg(any(target_os = "freebsd", target_os = "macos"))]
#[test]
fn aio_backend_write_roundtrip() {
    write_roundtrip("aio");
}

#[cfg(any(target_os = "freebsd", target_os = "macos"))]
#[test]
fn aio_backend_multi_span_roundtrip() {
    write_roundtrip_multi_span("aio");
}

#[test]
fn discard_writes_skips_io() {
    let dir = tempfile::tempdir().unwrap();
    let layout = test_layout(dir.path());
    // No ensure_storage — discard must not need files.
    let w = DiskWorker::spawn_with_options(true, "thread", DEFAULT_DISK_DEPTH).unwrap();
    assert!(w.discard_writes());
    let (tx, rx) = flume::unbounded();
    if let Err((e, _)) = w.submit_write(DiskWriteJob {
        index: 0,
        plen: 32,
        data: vec![0xab; 32],
        layout,
        reply: tx,
    }) {
        panic!("submit: {e}");
    }
    match rx.recv().unwrap() {
        HashOutcome::Ok { .. } => {}
        HashOutcome::HashFail { .. } | HashOutcome::WriteFail { .. } => {
            panic!("discard should Ok")
        }
    }
}

#[test]
fn depth_env_clamps() {
    // Just ensure function is total; don't mutate env in parallel tests heavily.
    let d = disk_depth_from_env();
    assert!((1..=DISK_DEPTH_MAX).contains(&d));
}

struct DeadBackend;

impl DiskWriteBackend for DeadBackend {
    fn submit(&self, job: DiskWriteJob) -> std::result::Result<(), (Error, DiskWriteJob)> {
        Err((Error::DiskWorkerStopped, job))
    }
}

#[test]
fn restart_after_dead_backend_then_write_ok() {
    let dir = tempfile::tempdir().unwrap();
    let layout = test_layout(dir.path());
    ensure_storage(&layout).unwrap();
    let w = DiskWorker::spawn_with_options(false, "thread", DEFAULT_DISK_DEPTH).unwrap();
    w.inject_backend_for_test(Arc::new(DeadBackend));
    assert_eq!(w.restart_count(), 0);

    let (tx, rx) = flume::unbounded();
    let data: Vec<u8> = (0..32).collect();
    if let Err((e, _)) = w.submit_write(DiskWriteJob {
        index: 0,
        plen: 32,
        data: data.clone(),
        layout: Arc::clone(&layout),
        reply: tx,
    }) {
        panic!("restart + submit should succeed: {e}");
    }
    assert_eq!(w.restart_count(), 1);
    match rx.recv().unwrap() {
        HashOutcome::Ok { data: d, .. } => assert_eq!(d, data),
        other => panic!("expected Ok after restart, got {other:?}"),
    }
}

#[test]
fn permanent_dead_after_max_restarts() {
    let w = DiskWorker::spawn_with_options(false, "thread", DEFAULT_DISK_DEPTH).unwrap();
    w.set_restart_state_for_test(
        MAX_DISK_RESTARTS,
        false,
        Some(Instant::now() - Duration::from_secs(60)),
    );
    w.inject_backend_for_test(Arc::new(DeadBackend));

    let (tx, _rx) = flume::unbounded();
    let layout = test_layout(tempfile::tempdir().unwrap().path());
    let err = w
        .submit_write(DiskWriteJob {
            index: 0,
            plen: 32,
            data: vec![0; 32],
            layout,
            reply: tx,
        })
        .unwrap_err()
        .0;
    assert!(w.is_permanently_dead(), "err={err}");
    // Next submit fails immediately without restart.
    let (tx2, _rx2) = flume::unbounded();
    let layout = test_layout(tempfile::tempdir().unwrap().path());
    let err2 = w
        .submit_write(DiskWriteJob {
            index: 0,
            plen: 32,
            data: vec![0; 32],
            layout,
            reply: tx2,
        })
        .unwrap_err()
        .0;
    assert!(matches!(err2, Error::DiskWorkerPermanent));
}

#[test]
fn is_backend_dead_detects_typed_errors() {
    assert!(is_backend_dead(&Error::DiskWorkerStopped));
    assert!(!is_backend_dead(&Error::DiskWorkerPermanent));
    assert!(!is_backend_dead(&Error::Msg("io_uring short write".into())));
}

#[test]
fn parse_backend_want_strict() {
    assert_eq!(parse_backend_want("auto").unwrap(), BackendWant::Auto);
    assert_eq!(parse_backend_want("").unwrap(), BackendWant::Auto);
    assert_eq!(parse_backend_want("thread").unwrap(), BackendWant::Thread);
    assert!(parse_backend_want("sync").is_err());
    assert!(parse_backend_want("pwrite").is_err());
    assert!(parse_backend_want("io_uring").is_err());
    assert!(DiskWorker::spawn_with_options(false, "nope", 8).is_err());
}
