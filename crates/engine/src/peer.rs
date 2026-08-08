//! Unified full-duplex async peer session (inbound + outbound).
//!
//! One established duplex session per connection; direction-specific policy is
//! expressed via [`PeerConfig`] flags (`allow_upload` / `allow_download`) and
//! establish-time differences (dial vs accept, PE IA remainder, `on_bound`).

mod config;
mod ctrl_scratch;
mod download;
mod duplex;
mod establish;
mod established;
mod helpers;
mod out_queue;
mod send;

pub use config::PeerConfig;
pub use establish::{run_inbound_peer, run_outbound_peer};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::EncryptionMode;
    use crate::disk::spans::FileLayout;
    use crate::hot::HotRegistry;
    use crate::hot::HotTorrent;
    use crate::runtime::DiskWorker;
    use crate::runtime::HashPool;
    use crate::session::PeerCrypto;
    use crate::upload::UploadOptions;
    use compio::net::TcpListener;
    use compio::runtime::spawn;
    use sha1::{Digest, Sha1};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    fn piece_hashes(data: &[u8], piece_length: u32) -> Vec<u8> {
        let mut hashes = Vec::new();
        let mut off = 0usize;
        while off < data.len() {
            let end = (off + piece_length as usize).min(data.len());
            let mut h = Sha1::new();
            h.update(&data[off..end]);
            hashes.extend_from_slice(&h.finalize());
            off = end;
        }
        hashes
    }

    fn seed_cfg(encryption: EncryptionMode) -> PeerConfig {
        PeerConfig {
            peer_id: *b"-sc0001-testseed!!!!",
            encryption,
            upload: UploadOptions::default(),
            allow_upload: true,
            allow_download: false,
            pipeline: 32,
            hash: None,
            on_piece: None,
            stop: None,
            on_bound: None,
            piece_count: None,
            wire_up: None,
            wire_down: None,
            on_upload: None,
            queue_outstanding: None,
            queue_target: None,
            peer_interested: None,
            peer_choking: None,
            am_interested: None,
            upload_pending: None,
            peer_have: None,
            crypto: None,
            client_label: None,
            ..PeerConfig::default()
        }
    }

    fn leech_cfg(
        encryption: EncryptionMode,
        hash: Arc<HashPool>,
        stop: Arc<AtomicBool>,
        crypto: Option<Arc<AtomicU8>>,
    ) -> PeerConfig {
        PeerConfig {
            peer_id: *b"-LC0001-testleech!!!",
            encryption,
            pipeline: 8,
            upload: UploadOptions::default(),
            allow_upload: false,
            allow_download: true,
            wire_down: None,
            wire_up: None,
            on_upload: None,
            queue_outstanding: None,
            queue_target: None,
            peer_interested: None,
            peer_choking: None,
            am_interested: None,
            upload_pending: None,
            peer_have: None,
            crypto,
            client_label: None,
            hash: Some(hash),
            on_piece: None,
            stop: Some(stop),
            on_bound: None,
            piece_count: None,
            ..PeerConfig::default()
        }
    }

    /// Full seed→leech download over one encryption mode (both sides same mode).
    async fn loopback_full_download(enc: EncryptionMode) {
        let seed_dir = tempfile::tempdir().unwrap();
        let leech_dir = tempfile::tempdir().unwrap();
        let piece_length = 32u32;
        let data: Vec<u8> = (0u8..96).collect();
        std::fs::write(seed_dir.path().join("f"), &data).unwrap();
        let hashes = piece_hashes(&data, piece_length);
        let ih = [0xCDu8; 20];
        let layout_seed = crate::disk::StorageLayout {
            data_root: seed_dir.path().to_path_buf(),
            piece_length,
            piece_count: 3,
            total_size: 96,
            files: vec![FileLayout {
                path: PathBuf::from("f"),
                size: 96,
                offset: 0,
                priority: 1,
            }],
        };
        let layout_leech = crate::disk::StorageLayout {
            data_root: leech_dir.path().to_path_buf(),
            piece_length,
            piece_count: 3,
            total_size: 96,
            files: vec![FileLayout {
                path: PathBuf::from("f"),
                size: 96,
                offset: 0,
                priority: 1,
            }],
        };
        crate::disk::ensure_storage(&layout_leech).unwrap();

        let seeder = HotTorrent::new_complete(1, ih, "t".into(), layout_seed, hashes.clone());
        let leecher = HotTorrent::new_empty(2, ih, "t".into(), layout_leech, hashes);
        let leecher = Arc::new(leecher);

        let mut reg = HotRegistry::new();
        reg.insert(seeder);
        let reg = Arc::new(parking_lot::RwLock::new(reg));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let reg2 = reg.clone();
        spawn(async move {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let _ = run_inbound_peer(stream, reg2, seed_cfg(enc)).await;
        })
        .detach();

        compio::time::sleep(Duration::from_millis(30)).await;

        let disk = Arc::new(DiskWorker::spawn().unwrap());
        let hash = Arc::new(HashPool::spawn_n(disk, 1).unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        run_outbound_peer(addr, leecher.clone(), leech_cfg(enc, hash, stop, None))
            .await
            .expect("async full download");
        assert!(leecher.is_complete());
        let got = std::fs::read(leech_dir.path().join("f")).unwrap();
        assert_eq!(got, data);
    }

    #[compio::test]
    async fn plain_loopback_full_download() {
        loopback_full_download(EncryptionMode::PreferPlain).await;
    }

    #[compio::test]
    async fn pe_rc4_loopback_full_download() {
        // PreferRc4 forces PE+RC4 on both sides (outbound always PE; seeder accepts).
        loopback_full_download(EncryptionMode::PreferRc4).await;
    }

    /// prefer-rc4: PE fails against plain-only seeder; plain retry completes.
    #[compio::test]
    async fn prefer_rc4_retry_to_plain_only_seeder() {
        let seed_dir = tempfile::tempdir().unwrap();
        let leech_dir = tempfile::tempdir().unwrap();
        let piece_length = 32u32;
        let data: Vec<u8> = (0u8..96).collect();
        std::fs::write(seed_dir.path().join("f"), &data).unwrap();
        let hashes = piece_hashes(&data, piece_length);
        let ih = [0xCCu8; 20];
        let layout_seed = crate::disk::StorageLayout {
            data_root: seed_dir.path().to_path_buf(),
            piece_length,
            piece_count: 3,
            total_size: 96,
            files: vec![FileLayout {
                path: PathBuf::from("f"),
                size: 96,
                offset: 0,
                priority: 1,
            }],
        };
        let layout_leech = crate::disk::StorageLayout {
            data_root: leech_dir.path().to_path_buf(),
            piece_length,
            piece_count: 3,
            total_size: 96,
            files: vec![FileLayout {
                path: PathBuf::from("f"),
                size: 96,
                offset: 0,
                priority: 1,
            }],
        };
        crate::disk::ensure_storage(&layout_leech).unwrap();

        let seeder = HotTorrent::new_complete(1, ih, "t".into(), layout_seed, hashes.clone());
        let leecher = HotTorrent::new_empty(2, ih, "t".into(), layout_leech, hashes);
        let leecher = Arc::new(leecher);

        let mut reg = HotRegistry::new();
        reg.insert(seeder);
        let reg = Arc::new(parking_lot::RwLock::new(reg));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let reg2 = reg.clone();
        spawn(async move {
            for _ in 0..4 {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let _ = run_inbound_peer(stream, reg2.clone(), seed_cfg(EncryptionMode::Off)).await;
            }
        })
        .detach();

        compio::time::sleep(Duration::from_millis(30)).await;

        let disk = Arc::new(DiskWorker::spawn().unwrap());
        let hash = Arc::new(HashPool::spawn_n(disk, 1).unwrap());
        let crypto = Arc::new(AtomicU8::new(PeerCrypto::Unknown as u8));
        let stop = Arc::new(AtomicBool::new(false));
        let lcfg = leech_cfg(EncryptionMode::PreferRc4, hash, stop, Some(crypto.clone()));
        run_outbound_peer(addr, leecher.clone(), lcfg)
            .await
            .expect("prefer-rc4 async should plain-retry onto plain-only seeder");
        assert!(leecher.is_complete());
        let got = std::fs::read(leech_dir.path().join("f")).unwrap();
        assert_eq!(got, data);
        assert_eq!(
            PeerCrypto::from_u8(crypto.load(Ordering::Relaxed)),
            PeerCrypto::Plain
        );
    }

    /// Production path: prefer-plain outbound vs RC4-only seeder must PE-retry and finish.
    #[compio::test]
    async fn prefer_plain_retry_to_require_rc4_seeder() {
        let seed_dir = tempfile::tempdir().unwrap();
        let leech_dir = tempfile::tempdir().unwrap();
        let piece_length = 32u32;
        let data: Vec<u8> = (0u8..96).collect();
        std::fs::write(seed_dir.path().join("f"), &data).unwrap();
        let hashes = piece_hashes(&data, piece_length);
        let ih = [0xAAu8; 20];
        let layout_seed = crate::disk::StorageLayout {
            data_root: seed_dir.path().to_path_buf(),
            piece_length,
            piece_count: 3,
            total_size: 96,
            files: vec![FileLayout {
                path: PathBuf::from("f"),
                size: 96,
                offset: 0,
                priority: 1,
            }],
        };
        let layout_leech = crate::disk::StorageLayout {
            data_root: leech_dir.path().to_path_buf(),
            piece_length,
            piece_count: 3,
            total_size: 96,
            files: vec![FileLayout {
                path: PathBuf::from("f"),
                size: 96,
                offset: 0,
                priority: 1,
            }],
        };
        crate::disk::ensure_storage(&layout_leech).unwrap();

        let seeder = HotTorrent::new_complete(1, ih, "t".into(), layout_seed, hashes.clone());
        let leecher = HotTorrent::new_empty(2, ih, "t".into(), layout_leech, hashes);
        let leecher = Arc::new(leecher);

        let mut reg = HotRegistry::new();
        reg.insert(seeder);
        let reg = Arc::new(parking_lot::RwLock::new(reg));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let reg2 = reg.clone();
        spawn(async move {
            for _ in 0..4 {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let _ =
                    run_inbound_peer(stream, reg2.clone(), seed_cfg(EncryptionMode::RequireRc4))
                        .await;
            }
        })
        .detach();

        compio::time::sleep(Duration::from_millis(30)).await;

        let disk = Arc::new(DiskWorker::spawn().unwrap());
        let hash = Arc::new(HashPool::spawn_n(disk, 1).unwrap());
        let crypto = Arc::new(AtomicU8::new(PeerCrypto::Unknown as u8));
        let stop = Arc::new(AtomicBool::new(false));
        let lcfg = leech_cfg(
            EncryptionMode::PreferPlain,
            hash,
            stop,
            Some(crypto.clone()),
        );
        run_outbound_peer(addr, leecher.clone(), lcfg)
            .await
            .expect("prefer-plain async should PE-retry onto RC4-only seeder");
        assert!(
            leecher.is_complete(),
            "async download incomplete after PE retry"
        );
        let got = std::fs::read(leech_dir.path().join("f")).unwrap();
        assert_eq!(got, data);
        let c = PeerCrypto::from_u8(crypto.load(Ordering::Relaxed));
        assert_eq!(
            c,
            PeerCrypto::Rc4,
            "async path must publish RC4 after prefer-plain → PE retry"
        );
    }

    #[compio::test]
    async fn plain_2m_loopback() {
        let seed_dir = tempfile::tempdir().unwrap();
        let leech_dir = tempfile::tempdir().unwrap();
        let piece_length = 32768u32;
        let data: Vec<u8> = (0..2_097_152u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(seed_dir.path().join("f"), &data).unwrap();
        let hashes = piece_hashes(&data, piece_length);
        let ih = [0xABu8; 20];
        let piece_count = (data.len() as u32 + piece_length - 1) / piece_length;
        let layout_seed = crate::disk::StorageLayout {
            data_root: seed_dir.path().to_path_buf(),
            piece_length,
            piece_count,
            total_size: data.len() as u64,
            files: vec![FileLayout {
                path: PathBuf::from("f"),
                size: data.len() as u64,
                offset: 0,
                priority: 1,
            }],
        };
        let layout_leech = crate::disk::StorageLayout {
            data_root: leech_dir.path().to_path_buf(),
            piece_length,
            piece_count,
            total_size: data.len() as u64,
            files: vec![FileLayout {
                path: PathBuf::from("f"),
                size: data.len() as u64,
                offset: 0,
                priority: 1,
            }],
        };
        crate::disk::ensure_storage(&layout_leech).unwrap();
        let seeder = HotTorrent::new_complete(1, ih, "t".into(), layout_seed, hashes.clone());
        let leecher = Arc::new(HotTorrent::new_empty(
            2,
            ih,
            "t".into(),
            layout_leech,
            hashes,
        ));
        let mut reg = HotRegistry::new();
        reg.insert(seeder);
        let reg = Arc::new(parking_lot::RwLock::new(reg));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let reg2 = reg.clone();
        spawn(async move {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let _ = run_inbound_peer(stream, reg2, seed_cfg(EncryptionMode::PreferPlain)).await;
        })
        .detach();
        compio::time::sleep(Duration::from_millis(30)).await;
        let disk = Arc::new(DiskWorker::spawn().unwrap());
        let hash = Arc::new(HashPool::spawn_n(disk, 2).unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        run_outbound_peer(
            addr,
            leecher.clone(),
            leech_cfg(EncryptionMode::PreferPlain, hash, stop, None),
        )
        .await
        .expect("2m download");
        assert!(leecher.is_complete());
        assert_eq!(std::fs::read(leech_dir.path().join("f")).unwrap(), data);
    }
}

#[cfg(test)]
mod pipe_tests {
    use crate::runtime::{desired_pipeline_blocks, MAX_PIPELINE, MIN_PIPELINE};

    #[test]
    fn pipe_scales_with_rate_bdp() {
        assert_eq!(
            desired_pipeline_blocks(0, 5.0, MIN_PIPELINE, MAX_PIPELINE),
            2
        );
        // ~1 MiB/s × 5 s / 16 KiB ≈ 320
        let fast = desired_pipeline_blocks(1024 * 1024, 5.0, MIN_PIPELINE, MAX_PIPELINE);
        assert!(fast >= 300, "fast peer pipe={fast}");
        assert!(fast <= MAX_PIPELINE);
        assert_eq!(
            desired_pipeline_blocks(100 * 1024 * 1024, 5.0, MIN_PIPELINE, MAX_PIPELINE),
            MAX_PIPELINE
        );
    }
}
