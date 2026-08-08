//! Lightweight catalog / list microbenchmarks (Phase 6).

use std::io::Write;
use std::path::Path;
use std::time::Instant;

use sha1::{Digest, Sha1};

use crate::catalog::{Catalog, TorrentInsert};
use crate::error::{Error, Result};
use crate::metainfo::{Metainfo, TorrentFile};

#[derive(Debug, Clone)]
pub struct BenchReport {
    pub name: String,
    pub iterations: u32,
    pub count: u32,
    pub elapsed_ms: f64,
    pub ops_per_sec: f64,
    pub notes: String,
}

/// Insert `count` synthetic single-file torrents and time `list_torrents`.
pub fn bench_catalog_fill_and_list(
    db: &Path,
    count: u32,
    list_iters: u32,
) -> Result<Vec<BenchReport>> {
    if let Some(parent) = db.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = std::fs::remove_file(db);
    let mut cat = Catalog::open(db)?;

    let t0 = Instant::now();
    for i in 0..count {
        let m = synthetic_metainfo(i);
        let ins = TorrentInsert::from_metainfo(m, format!("/tmp/seedchamp-bench/{i}"));
        cat.insert_torrent(&ins)?;
    }
    let fill_ms = t0.elapsed().as_secs_f64() * 1000.0;

    let t1 = Instant::now();
    let iters = list_iters.max(1);
    let mut n = 0usize;
    for _ in 0..iters {
        n = cat.list_torrents()?.len();
    }
    let list_ms = t1.elapsed().as_secs_f64() * 1000.0;

    Ok(vec![
        BenchReport {
            name: "catalog_insert".into(),
            iterations: 1,
            count,
            elapsed_ms: fill_ms,
            ops_per_sec: if fill_ms > 0.0 {
                (count as f64) / (fill_ms / 1000.0)
            } else {
                0.0
            },
            notes: format!("synthetic torrents into {}", db.display()),
        },
        BenchReport {
            name: "catalog_list".into(),
            iterations: iters,
            count: n as u32,
            elapsed_ms: list_ms,
            ops_per_sec: if list_ms > 0.0 {
                (iters as f64) / (list_ms / 1000.0)
            } else {
                0.0
            },
            notes: format!("{n} rows × {iters} iterations"),
        },
    ])
}

/// Time filtered list only against an existing catalog.
pub fn bench_list_existing(db: &Path, iters: u32) -> Result<BenchReport> {
    let cat = Catalog::open(db)?;
    let iters = iters.max(1);
    let t0 = Instant::now();
    let mut n = 0usize;
    for _ in 0..iters {
        n = cat.list_torrents()?.len();
    }
    let ms = t0.elapsed().as_secs_f64() * 1000.0;
    Ok(BenchReport {
        name: "catalog_list_existing".into(),
        iterations: iters,
        count: n as u32,
        elapsed_ms: ms,
        ops_per_sec: if ms > 0.0 {
            (iters as f64) / (ms / 1000.0)
        } else {
            0.0
        },
        notes: format!("{} rows", n),
    })
}

/// Process RSS in bytes (Linux `/proc/self/status`); None if unavailable.
#[inline]
pub fn current_rss_bytes() -> Option<u64> {
    crate::process_metrics::current_rss_bytes()
}

pub fn print_report(w: &mut dyn Write, reports: &[BenchReport]) -> Result<()> {
    writeln!(w, "seedchamp bench").map_err(|e| Error::Msg(e.to_string()))?;
    if let Some(rss) = current_rss_bytes() {
        writeln!(w, "  rss: {:.1} MiB", rss as f64 / (1024.0 * 1024.0))
            .map_err(|e| Error::Msg(e.to_string()))?;
    }
    for r in reports {
        writeln!(
            w,
            "  {:<22} count={:<6} iters={:<5} {:>8.2} ms  {:>10.1} ops/s  {}",
            r.name, r.count, r.iterations, r.elapsed_ms, r.ops_per_sec, r.notes
        )
        .map_err(|e| Error::Msg(e.to_string()))?;
    }
    Ok(())
}

fn synthetic_metainfo(i: u32) -> Metainfo {
    // Distinct 20-byte infohash-like pieces blob; real infohash comes from bencode parse.
    // Build minimal torrent bytes and re-parse for correctness.
    let name = format!("bench-{i:05}.bin");
    let length = 1024u64 + (i as u64 % 1000);
    let piece_length = 16384u32;
    let mut piece_data = vec![0u8; length as usize];
    for (j, b) in piece_data.iter_mut().enumerate() {
        *b = ((i as usize + j) % 251) as u8;
    }
    let mut pieces = Vec::new();
    let mut off = 0usize;
    while off < piece_data.len() {
        let end = (off + piece_length as usize).min(piece_data.len());
        let mut h = Sha1::new();
        h.update(&piece_data[off..end]);
        pieces.extend_from_slice(&h.finalize());
        off = end;
    }
    let piece_count = (pieces.len() / 20) as u32;

    // Hand-build bencode and parse so infohash is real.
    let mut info = Vec::new();
    info.extend_from_slice(format!("d6:lengthi{length}e4:name{}:", name.len()).as_bytes());
    info.extend_from_slice(name.as_bytes());
    info.extend_from_slice(
        format!("12:piece lengthi{piece_length}e6:pieces{}:", pieces.len()).as_bytes(),
    );
    info.extend_from_slice(&pieces);
    info.extend_from_slice(b"e");
    let mut root = Vec::new();
    root.extend_from_slice(b"d8:announce10:http://b/x4:info");
    root.extend_from_slice(&info);
    root.extend_from_slice(b"e");
    Metainfo::parse_bytes(&root).unwrap_or_else(|_| Metainfo {
        infohash: {
            let mut h = [0u8; 20];
            h[0] = (i >> 24) as u8;
            h[1] = (i >> 16) as u8;
            h[2] = (i >> 8) as u8;
            h[3] = i as u8;
            h
        },
        name,
        piece_length,
        piece_count,
        total_size: length,
        pieces,
        files: vec![TorrentFile {
            path: std::path::PathBuf::from(format!("bench-{i:05}.bin")),
            size: length,
            offset: 0,
        }],
        is_multi_file: false,
        private: false,
        trackers: vec![(0, "http://b/x".into())],
        announce: Some("http://b/x".into()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_small() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("b.sqlite");
        let reports = bench_catalog_fill_and_list(&db, 20, 5).unwrap();
        assert_eq!(reports.len(), 2);
        assert!(reports[0].count == 20);
        assert!(reports[1].count == 20);
    }
}
