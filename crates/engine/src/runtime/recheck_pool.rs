//! Parallel full-torrent recheck via [`HashPool`].

use std::sync::Arc;

use crossbeam_channel::unbounded;

use crate::catalog::Catalog;
use crate::error::{Error, Result};
use crate::hash::{
    emit_start_progress, finish_recheck, maybe_progress, prepare_recheck, progress_step,
    RecheckProgress, RecheckReport,
};

use crate::runtime::hash_worker::{HashPool, RecheckJob, RecheckPieceResult};

/// Fan all pieces to the hash pool (windowed pread + SHA-1 on workers), then
/// write the bitfield. Caller (mutate) still waits for completion so catalog
/// commits stay ordered with start/stop.
pub fn recheck_torrent_with_pool(
    catalog: &mut Catalog,
    torrent_id: i64,
    pool: &HashPool,
    mut on_progress: impl FnMut(RecheckProgress),
) -> Result<RecheckReport> {
    let prepared = prepare_recheck(catalog, torrent_id)?;
    let pc = prepared.piece_count;
    emit_start_progress(torrent_id, pc, &mut on_progress);

    if pc == 0 {
        return finish_recheck(catalog, torrent_id, 0, &[], 0, 0, 0);
    }

    let layout = Arc::new(prepared.layout);
    let (tx, rx) = unbounded::<RecheckPieceResult>();

    for i in 0..pc {
        let mut expected = [0u8; 20];
        expected.copy_from_slice(&prepared.hashes[i as usize * 20..(i as usize + 1) * 20]);
        pool.submit_recheck(RecheckJob {
            index: i,
            expected,
            layout: Arc::clone(&layout),
            reply: tx.clone(),
        })?;
    }
    drop(tx);

    let mut have = vec![false; pc as usize];
    let mut good = 0u32;
    let mut bad = 0u32;
    let mut missing = 0u32;
    let step = progress_step(pc);

    for checked in 1..=pc {
        let r = rx
            .recv()
            .map_err(|_| Error::Msg("recheck: hash workers stopped mid-run".into()))?;
        match r {
            RecheckPieceResult::Good { index } => {
                if (index as usize) < have.len() {
                    have[index as usize] = true;
                }
                good += 1;
            }
            RecheckPieceResult::Bad { .. } => bad += 1,
            RecheckPieceResult::Missing { .. } => missing += 1,
        }
        maybe_progress(
            torrent_id,
            pc,
            checked,
            good,
            bad,
            missing,
            step,
            &mut on_progress,
        );
    }

    finish_recheck(catalog, torrent_id, pc, &have, good, bad, missing)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::TorrentInsert;
    use crate::metainfo::Metainfo;
    use crate::runtime::disk_worker::{DiskWorker, DEFAULT_DISK_DEPTH};
    use crate::runtime::hash_worker::HashPool;
    use sha1::{Digest, Sha1};
    use std::sync::Arc;

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
    fn recheck_parallel_pool_good() {
        let dir = tempfile::tempdir().unwrap();
        let payload: Vec<u8> = (0u16..500).map(|i| (i % 256) as u8).collect();
        let torrent_bytes = make_torrent_bytes(&payload, 64);
        let m = Metainfo::parse_bytes(&torrent_bytes).unwrap();
        let pc = m.piece_count;

        std::fs::write(dir.path().join("data.bin"), &payload).unwrap();
        let db = dir.path().join("c.sqlite");
        let mut cat = Catalog::open(&db).unwrap();
        let mut ins = TorrentInsert::from_metainfo(m, dir.path().display().to_string());
        ins.source_torrent = Some("x.torrent".into());
        let id = cat.insert_torrent(&ins).unwrap().id();

        let disk =
            Arc::new(DiskWorker::spawn_with_options(false, "thread", DEFAULT_DISK_DEPTH).unwrap());
        let pool = HashPool::spawn_n(disk, 4).unwrap();
        let report = recheck_torrent_with_pool(&mut cat, id, &pool, |_| {}).unwrap();
        assert_eq!(report.good, pc);
        assert!(report.complete);
    }
}
