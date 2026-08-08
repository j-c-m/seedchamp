//! Scan Transmission session root (`torrents/` + `resume/`) and import.

use std::fs;
use std::path::{Path, PathBuf};

use seedchamp_engine::{Catalog, InsertOutcome, Metainfo, Result, TorrentInsert};

use crate::common::{
    file_mtime_unix, is_infohash_torrent_name, strip_trailing_torrent_name, ImportOptions,
    ImportReport,
};
use crate::transmission::resume::parse_transmission_resume;

/// Import a Transmission config/session directory into the catalog at `db_path`.
///
/// Expects `session_dir/torrents/*.torrent` and optional matching
/// `session_dir/resume/{infohash}.resume`.
pub fn import_transmission(
    session_dir: &Path,
    db_path: &Path,
    dry_run: bool,
) -> Result<ImportReport> {
    import_transmission_with(
        session_dir,
        db_path,
        ImportOptions {
            dry_run,
            ..Default::default()
        },
    )
}

pub fn import_transmission_with(
    session_dir: &Path,
    db_path: &Path,
    opts: ImportOptions,
) -> Result<ImportReport> {
    if !session_dir.is_dir() {
        return Err(format!("not a directory: {}", session_dir.display()).into());
    }

    let torrents_dir = session_dir.join("torrents");
    if !torrents_dir.is_dir() {
        return Err(format!(
            "transmission session missing torrents/: {}",
            torrents_dir.display()
        )
        .into());
    }

    let resume_dir = session_dir.join("resume");
    let mut report = ImportReport::default();

    let mut torrent_paths: Vec<PathBuf> = Vec::new();
    for ent in fs::read_dir(&torrents_dir)? {
        let ent = ent?;
        let name = ent.file_name();
        let name = name.to_string_lossy();
        if is_infohash_torrent_name(&name) {
            torrent_paths.push(ent.path());
        }
    }
    torrent_paths.sort();
    report.scanned = torrent_paths.len() as u32;

    if opts.dry_run {
        for p in &torrent_paths {
            match Metainfo::parse_file(p) {
                Ok(_) => report.imported += 1,
                Err(e) => {
                    report.errors.push(format!("{}: {e}", p.display()));
                    report.skipped += 1;
                }
            }
        }
        return Ok(report);
    }

    let mut catalog = Catalog::open(db_path)?;

    for torrent_path in torrent_paths {
        match import_one(&mut catalog, &torrent_path, &resume_dir, &opts) {
            Ok(r) => {
                match r.kind {
                    ImportOneKind::Inserted => report.imported += 1,
                    ImportOneKind::Exists { updated } => {
                        report.skipped += 1;
                        if updated {
                            report.updated += 1;
                        }
                    }
                }
                report.uploaded_bytes = report.uploaded_bytes.saturating_add(r.uploaded);
                report.downloaded_bytes = report.downloaded_bytes.saturating_add(r.downloaded);
                if r.uploaded > 0 || r.downloaded > 0 {
                    report.with_transfer_stats += 1;
                }
            }
            Err(e) => {
                report
                    .errors
                    .push(format!("{}: {e}", torrent_path.display()));
                report.skipped += 1;
            }
        }
    }

    Ok(report)
}

enum ImportOneKind {
    Inserted,
    Exists { updated: bool },
}

struct ImportOneResult {
    kind: ImportOneKind,
    uploaded: u64,
    downloaded: u64,
}

fn find_resume_path(resume_dir: &Path, infohash_hex: &str) -> Option<PathBuf> {
    if !resume_dir.is_dir() {
        return None;
    }
    let lower = infohash_hex.to_ascii_lowercase();
    let upper = infohash_hex.to_ascii_uppercase();
    for stem in [&lower, &upper, infohash_hex] {
        for name in [format!("{stem}.resume"), format!("{stem}.torrent.resume")] {
            let p = resume_dir.join(&name);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

fn import_one(
    catalog: &mut Catalog,
    torrent_path: &Path,
    resume_dir: &Path,
    opts: &ImportOptions,
) -> Result<ImportOneResult> {
    let torrent_bytes = fs::read(torrent_path)
        .map_err(|e| seedchamp_engine::Error::Path(torrent_path.to_path_buf(), e.to_string()))?;
    let metainfo = Metainfo::parse_bytes(&torrent_bytes)?;
    let ih = metainfo.infohash_hex();

    let tr = if let Some(rp) = find_resume_path(resume_dir, &ih) {
        let bytes = fs::read(&rp)?;
        parse_transmission_resume(&bytes, metainfo.piece_count).unwrap_or_default()
    } else {
        Default::default()
    };

    let mut data_root = tr
        .data_root
        .clone()
        .unwrap_or_else(|| opts.default_data_root.clone());
    if metainfo.is_multi_file {
        data_root = strip_trailing_torrent_name(&data_root, &metainfo.name);
    }

    let mut ins = TorrentInsert::from_metainfo(metainfo, data_root);
    ins.metainfo_blob = Some(torrent_bytes);
    ins.source_torrent = Some(torrent_path.display().to_string());
    ins.complete = tr.complete;
    ins.have_count = tr.have_count;
    ins.bitfield = tr.bitfield;
    ins.uploaded = tr.uploaded;
    ins.downloaded = tr.downloaded;
    ins.file_priorities = tr.file_priorities;
    ins.created_at = tr.created_at.or_else(|| file_mtime_unix(torrent_path));

    if ins.complete {
        ins.state = "stopped".into();
        ins.finished_at = tr
            .finished_at
            .or(ins.created_at)
            .or_else(|| Some(TorrentInsert::now_unix()));
    } else if let Some(fin) = tr.finished_at {
        ins.finished_at = Some(fin);
    }

    if opts.start_after {
        ins.want_start = true;
        ins.state = "started".into();
    }

    let uploaded = ins.uploaded;
    let downloaded = ins.downloaded;

    match catalog.insert_torrent(&ins)? {
        InsertOutcome::Inserted { .. } => Ok(ImportOneResult {
            kind: ImportOneKind::Inserted,
            uploaded,
            downloaded,
        }),
        InsertOutcome::Restored { id } => {
            catalog.update_import_meta(
                id,
                ins.created_at,
                ins.finished_at,
                Some(ins.uploaded),
                Some(ins.downloaded),
            )?;
            let _ = catalog.ensure_tracker_key(id);
            if !ins.file_priorities.is_empty() {
                catalog.set_file_priorities(id, &ins.file_priorities)?;
            }
            if let Some(ref blob) = ins.metainfo_blob {
                let _ = catalog.set_metainfo_blob(id, blob);
            }
            Ok(ImportOneResult {
                kind: ImportOneKind::Inserted,
                uploaded,
                downloaded,
            })
        }
        InsertOutcome::Exists { id } => {
            catalog.update_import_meta(
                id,
                ins.created_at,
                ins.finished_at,
                Some(ins.uploaded),
                Some(ins.downloaded),
            )?;
            let _ = catalog.ensure_tracker_key(id);
            if !ins.file_priorities.is_empty() {
                catalog.set_file_priorities(id, &ins.file_priorities)?;
            }
            if let Some(ref blob) = ins.metainfo_blob {
                let _ = catalog.set_metainfo_blob(id, blob);
            }
            Ok(ImportOneResult {
                kind: ImportOneKind::Exists { updated: true },
                uploaded,
                downloaded,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use seedchamp_engine::Metainfo;

    fn sample_torrent_bytes() -> Vec<u8> {
        let pieces = vec![0u8; 20];
        let mut info = Vec::new();
        info.extend_from_slice(b"d6:lengthi1e4:name4:test12:piece lengthi16384e6:pieces20:");
        info.extend_from_slice(&pieces);
        info.extend_from_slice(b"e");
        let mut root = Vec::new();
        root.extend_from_slice(b"d8:announce8:http://x4:info");
        root.extend_from_slice(&info);
        root.extend_from_slice(b"e");
        root
    }

    #[test]
    fn transmission_layout_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let torrents = dir.path().join("torrents");
        let resume = dir.path().join("resume");
        fs::create_dir_all(&torrents).unwrap();
        fs::create_dir_all(&resume).unwrap();

        let root = sample_torrent_bytes();
        let m = Metainfo::parse_bytes(&root).unwrap();
        let ih = m.infohash_hex();
        let tpath = torrents.join(format!("{}.torrent", ih.to_ascii_lowercase()));
        fs::write(&tpath, &root).unwrap();

        let benc = b"d11:destination3:/dl10:added-datei1700000000e9:done-datei1700000500e10:downloadedi50e8:uploadedi200e6:pausedi1e8:progressd6:blocks3:all6:pieces3:allee";
        fs::write(resume.join(format!("{ih}.resume")), benc).unwrap();

        let report = import_transmission(dir.path(), &dir.path().join("x.sqlite"), true).unwrap();
        assert_eq!(report.scanned, 1);
        assert_eq!(report.imported, 1);

        let db = dir.path().join("cat.sqlite");
        let report = import_transmission(dir.path(), &db, false).unwrap();
        assert_eq!(report.imported, 1);
        assert_eq!(report.errors.len(), 0);
        assert_eq!(report.uploaded_bytes, 200);
        assert_eq!(report.with_transfer_stats, 1);

        let cat = Catalog::open(&db).unwrap();
        let list = cat.list_torrents().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "test");
        assert!(list[0].complete);
        assert_eq!(list[0].data_root.as_deref(), Some("/dl"));
        assert_eq!(list[0].created_at, 1_700_000_000);
        assert_eq!(list[0].uploaded, 200);

        let report2 = import_transmission(dir.path(), &db, false).unwrap();
        assert_eq!(report2.skipped, 1);
        assert_eq!(report2.updated, 1);
        assert_eq!(report2.imported, 0);
    }

    #[test]
    fn missing_torrents_dir_errors() {
        let dir = tempfile::tempdir().unwrap();
        let err = import_transmission(dir.path(), &dir.path().join("x.sqlite"), true).unwrap_err();
        assert!(err.to_string().contains("torrents"));
    }
}
