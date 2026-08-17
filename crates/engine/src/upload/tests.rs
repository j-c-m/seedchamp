use super::*;
use crate::disk::spans::FileLayout;
use crate::disk::{with_peer_fd_cache, StorageLayout};
use std::path::PathBuf;

#[compio::test]
async fn begin_upload_fills_payload() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("f");
    let data: Vec<u8> = (0..80).collect();
    std::fs::write(&p, &data).unwrap();
    let layout = StorageLayout {
        data_root: dir.path().to_path_buf(),
        piece_length: 64,
        piece_count: 2,
        total_size: 80,
        files: vec![FileLayout {
            path: PathBuf::from("f"),
            size: 80,
            offset: 0,
            priority: 1,
        }],
    };
    with_peer_fd_cache(|c| c.clear());
    let mut scratch = vec![0u8; UPLOAD_SCRATCH_LEN];
    let opts = UploadOptions {
        backend: ResolvedUploadBackend::Compio,
    };
    begin_upload(
        &layout,
        UploadBlock {
            index: 0,
            begin: 0,
            length: 32,
        },
        None,
        opts,
        &mut scratch,
    )
    .await
    .unwrap();
    assert_eq!(scratch[4], 7);
    let msg_len = u32::from_be_bytes(scratch[0..4].try_into().unwrap());
    assert_eq!(msg_len, 9 + 32);
    assert_eq!(
        &scratch[PIECE_HEADER_LEN..PIECE_HEADER_LEN + 32],
        &data[..32]
    );
    assert_eq!(with_peer_fd_cache(|c| c.len()), 1);
}

#[test]
fn upload_backend_parse_and_resolve() {
    assert_eq!(UploadBackend::parse("auto").unwrap(), UploadBackend::Auto);
    assert_eq!(
        UploadBackend::parse("compio").unwrap(),
        UploadBackend::Compio
    );
    assert_eq!(
        UploadBackend::parse("pread").unwrap().resolve().unwrap(),
        ResolvedUploadBackend::Pread
    );
    assert_eq!(
        UploadBackend::Compio.resolve().unwrap(),
        ResolvedUploadBackend::Compio
    );
    assert!(UploadBackend::parse("async").is_err());
    assert!(UploadBackend::parse("uring").is_err());
    assert!(UploadBackend::parse("aio").is_err());
}

#[test]
fn piece_header_layout() {
    let h = read::build_piece_header(1, 0x100, 0x4000);
    assert_eq!(h.len(), PIECE_HEADER_LEN);
    assert_eq!(h[4], 7);
    let msg_len = u32::from_be_bytes(h[0..4].try_into().unwrap());
    assert_eq!(msg_len, 9 + 0x4000);
}
