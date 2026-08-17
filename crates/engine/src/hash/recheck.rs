//! Windowed piece recheck against stored SHA-1 hashes.
//!
//! Serial path lives here. Parallel fan-out via [`crate::runtime::HashPool`] is in
//! [`crate::runtime::recheck_torrent_with_pool`].

use sha1::{Digest, Sha1};

use crate::catalog::Catalog;
use crate::disk::{hash_piece_windowed, FdCache, StorageLayout};
use crate::error::{Error, Result};

/// Result of a full torrent recheck.
#[derive(Debug, Clone)]
pub struct RecheckReport {
    pub torrent_id: i64,
    pub piece_count: u32,
    pub good: u32,
    pub bad: u32,
    pub missing: u32,
    pub complete: bool,
}

/// Live progress while a recheck is running (HAVE can count up from 0).
#[derive(Debug, Clone, Copy)]
pub struct RecheckProgress {
    pub torrent_id: i64,
    pub piece_count: u32,
    /// Pieces examined so far (0…piece_count).
    pub checked: u32,
    /// Pieces that matched so far (drives HAVE display).
    pub good: u32,
    pub bad: u32,
    pub missing: u32,
}

/// Recheck all pieces for `torrent_id`, update catalog bitfield (serial).
pub fn recheck_torrent(catalog: &mut Catalog, torrent_id: i64) -> Result<RecheckReport> {
    let prepared = prepare_recheck(catalog, torrent_id)?;
    let mut cache = FdCache::default_cache();
    let mut have = vec![false; prepared.piece_count as usize];
    let mut good = 0u32;
    let mut bad = 0u32;
    let mut missing = 0u32;
    let pc = prepared.piece_count;

    for i in 0..pc {
        let expected = &prepared.hashes[i as usize * 20..(i as usize + 1) * 20];
        match check_one_piece(&mut cache, &prepared.layout, i, expected) {
            Ok(true) => {
                have[i as usize] = true;
                good += 1;
            }
            Ok(false) => {
                bad += 1;
            }
            Err(e) => {
                tracing::debug!(piece = i, error = %e, "recheck piece unreadable");
                missing += 1;
            }
        }
    }

    finish_recheck(catalog, torrent_id, pc, &have, good, bad, missing)
}

/// Inputs for a recheck run (serial or parallel).
pub struct RecheckPrepared {
    pub layout: StorageLayout,
    pub hashes: Vec<u8>,
    pub piece_count: u32,
}

/// Load layout/hashes and set catalog state to `checking`.
pub fn prepare_recheck(catalog: &mut Catalog, torrent_id: i64) -> Result<RecheckPrepared> {
    let layout = catalog.load_storage_layout(torrent_id)?;
    let hashes = catalog.load_piece_hashes(torrent_id)?;
    let pc = layout.piece_count;
    if hashes.len() != pc as usize * 20 {
        return Err(Error::Msg(format!(
            "piece hash blob size {} != piece_count {pc} * 20",
            hashes.len()
        )));
    }
    catalog.set_torrent_state(torrent_id, "checking")?;
    Ok(RecheckPrepared {
        layout,
        hashes,
        piece_count: pc,
    })
}

/// Persist bitfield and build the final report.
pub fn finish_recheck(
    catalog: &mut Catalog,
    torrent_id: i64,
    pc: u32,
    have: &[bool],
    good: u32,
    bad: u32,
    missing: u32,
) -> Result<RecheckReport> {
    catalog.set_bitfield_from_recheck(torrent_id, pc, have)?;
    Ok(RecheckReport {
        torrent_id,
        piece_count: pc,
        good,
        bad,
        missing,
        complete: good == pc && pc > 0,
    })
}

pub fn emit_start_progress(
    torrent_id: i64,
    pc: u32,
    on_progress: &mut impl FnMut(RecheckProgress),
) {
    on_progress(RecheckProgress {
        torrent_id,
        piece_count: pc,
        checked: 0,
        good: 0,
        bad: 0,
        missing: 0,
    });
}

pub fn progress_step(pc: u32) -> u32 {
    ((pc as usize) / 200).max(1) as u32
}

pub fn maybe_progress(
    torrent_id: i64,
    pc: u32,
    checked: u32,
    good: u32,
    bad: u32,
    missing: u32,
    step: u32,
    on_progress: &mut impl FnMut(RecheckProgress),
) {
    if checked == 1 || checked == pc || checked.is_multiple_of(step) {
        on_progress(RecheckProgress {
            torrent_id,
            piece_count: pc,
            checked,
            good,
            bad,
            missing,
        });
    }
}

fn check_one_piece(
    cache: &mut FdCache,
    layout: &StorageLayout,
    index: u32,
    expected: &[u8],
) -> Result<bool> {
    let mut hasher = Sha1::new();
    hash_piece_windowed(cache, layout, index, &mut hasher)?;
    let digest = hasher.finalize();
    Ok(digest.as_slice() == expected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::TorrentInsert;
    use crate::metainfo::Metainfo;
    use sha1::{Digest, Sha1};
    use std::io::Write;

    fn make_torrent_bytes(payload: &[u8], piece_length: u32) -> (Vec<u8>, [u8; 20]) {
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

        let m = Metainfo::parse_bytes(&root).unwrap();
        (root, m.infohash)
    }

    #[test]
    fn recheck_good_and_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let payload: Vec<u8> = (0u8..200).collect();
        let (torrent_bytes, _) = make_torrent_bytes(&payload, 64);
        let m = Metainfo::parse_bytes(&torrent_bytes).unwrap();

        let data_path = dir.path().join("data.bin");
        std::fs::File::create(&data_path)
            .unwrap()
            .write_all(&payload)
            .unwrap();

        let db = dir.path().join("c.sqlite");
        let mut cat = Catalog::open(&db).unwrap();
        let mut ins = TorrentInsert::from_metainfo(m.clone(), dir.path().display().to_string());
        ins.source_torrent = Some("x.torrent".into());
        let id = cat.insert_torrent(&ins).unwrap().id();

        let report = recheck_torrent(&mut cat, id).unwrap();
        assert_eq!(report.good, m.piece_count);
        assert_eq!(report.bad, 0);
        assert!(report.complete);
        let (complete, _, have) = cat.load_bitfield_bytes(id).unwrap();
        assert!(complete);
        assert_eq!(have, m.piece_count);

        let mut bad = payload.clone();
        bad[10] ^= 0xff;
        std::fs::write(&data_path, &bad).unwrap();
        let report = recheck_torrent(&mut cat, id).unwrap();
        assert!(!report.complete);
        assert!(report.bad >= 1);
        let (complete, _, _) = cat.load_bitfield_bytes(id).unwrap();
        assert!(!complete);
    }
}
