//! Scan rtorrent session directory and import torrents.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use seedchamp_engine::{Catalog, InsertOutcome, Metainfo, Result, TorrentInsert};

use crate::resume::parse_resume;
use crate::rtorrent_side::parse_rtorrent;

/// Import options.
#[derive(Debug, Clone)]
pub struct ImportOptions {
    pub dry_run: bool,
    pub start_after: bool,
    /// Default data root when `.rtorrent` has no directory.
    pub default_data_root: String,
}

impl Default for ImportOptions {
    fn default() -> Self {
        Self {
            dry_run: false,
            start_after: false,
            default_data_root: ".".into(),
        }
    }
}

#[derive(Debug, Default)]
pub struct ImportReport {
    pub scanned: u32,
    pub imported: u32,
    /// Already in catalog; metadata may have been refreshed (timestamps/stats).
    pub skipped: u32,
    /// Existing rows whose created_at / finished_at / stats were updated.
    pub updated: u32,
    /// Sum of lifetime upload bytes applied from session (`.rtorrent` / resume).
    pub uploaded_bytes: u64,
    /// Sum of lifetime download bytes applied from session.
    pub downloaded_bytes: u64,
    /// How many torrents had non-zero up or down totals in the session files.
    pub with_transfer_stats: u32,
    pub errors: Vec<String>,
}

/// Import an rtorrent session directory into the catalog at `db_path`.
pub fn import_session(session_dir: &Path, db_path: &Path, dry_run: bool) -> Result<ImportReport> {
    import_session_with(
        session_dir,
        db_path,
        ImportOptions {
            dry_run,
            ..Default::default()
        },
    )
}

pub fn import_session_with(
    session_dir: &Path,
    db_path: &Path,
    opts: ImportOptions,
) -> Result<ImportReport> {
    if !session_dir.is_dir() {
        return Err(format!("not a directory: {}", session_dir.display()).into());
    }

    let mut report = ImportReport::default();
    let entries = fs::read_dir(session_dir)?;

    let mut torrent_paths: Vec<PathBuf> = Vec::new();
    for ent in entries {
        let ent = ent?;
        let name = ent.file_name();
        let name = name.to_string_lossy();
        if is_session_torrent_name(&name) {
            torrent_paths.push(ent.path());
        }
    }
    torrent_paths.sort();

    report.scanned = torrent_paths.len() as u32;

    if opts.dry_run {
        for p in &torrent_paths {
            match Metainfo::parse_file(p) {
                Ok(m) => {
                    // count as would-import
                    let _ = m;
                    report.imported += 1;
                }
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
        match import_one(&mut catalog, &torrent_path, &opts) {
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

fn file_mtime_unix(path: &Path) -> Option<i64> {
    let meta = fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    Some(
        modified
            .duration_since(SystemTime::UNIX_EPOCH)
            .ok()?
            .as_secs() as i64,
    )
}

/// If `data_root` ends with `name` (rtorrent multi-file root), return parent.
fn strip_trailing_torrent_name(data_root: &str, name: &str) -> String {
    let p = Path::new(data_root);
    if p.file_name().and_then(|s| s.to_str()) == Some(name) {
        if let Some(parent) = p.parent() {
            if !parent.as_os_str().is_empty() {
                return parent.display().to_string();
            }
        }
    }
    data_root.to_string()
}

fn import_one(
    catalog: &mut Catalog,
    torrent_path: &Path,
    opts: &ImportOptions,
) -> Result<ImportOneResult> {
    let torrent_bytes = fs::read(torrent_path)
        .map_err(|e| seedchamp_engine::Error::Path(torrent_path.to_path_buf(), e.to_string()))?;
    let metainfo = Metainfo::parse_bytes(&torrent_bytes)?;
    let base = torrent_path.to_string_lossy();
    // sidecars: file.torrent.rtorrent and file.torrent.libtorrent_resume
    // our files are named HASH.torrent so sidecars are HASH.torrent.rtorrent
    let rtorrent_path = PathBuf::from(format!("{base}.rtorrent"));
    let resume_path = PathBuf::from(format!("{base}.libtorrent_resume"));

    let side = if rtorrent_path.is_file() {
        let bytes = fs::read(&rtorrent_path)?;
        parse_rtorrent(&bytes).unwrap_or_default()
    } else {
        Default::default()
    };

    let resume = if resume_path.is_file() {
        let bytes = fs::read(&resume_path)?;
        parse_resume(&bytes, metainfo.piece_count).unwrap_or_default()
    } else {
        Default::default()
    };

    let mut data_root = side
        .data_root()
        .unwrap_or_else(|| opts.default_data_root.clone());
    // rtorrent `d.directory.set` appends the torrent name for multi-file, so the
    // session `directory` key is already `…/TorrentName`. Our metainfo paths also
    // include `name/` — strip the trailing name so we do not double-nest.
    if metainfo.is_multi_file {
        data_root = strip_trailing_torrent_name(&data_root, &metainfo.name);
    }

    let mut ins = TorrentInsert::from_metainfo(metainfo, data_root);
    ins.metainfo_blob = Some(torrent_bytes);
    ins.source_torrent = Some(torrent_path.display().to_string());
    ins.complete = resume.complete;
    ins.have_count = resume.have_count;
    ins.bitfield = resume.bitfield;
    // Lifetime totals live in `.rtorrent` as total_uploaded / total_downloaded
    // (libtorrent_resume usually has no uploaded/downloaded keys).
    ins.uploaded = side.total_uploaded.unwrap_or(0).max(resume.uploaded);
    ins.downloaded = side.total_downloaded.unwrap_or(0).max(resume.downloaded);
    ins.file_priorities = resume.file_priorities;

    // created_at: rtorrent timestamps, else session file mtime (not "import now").
    ins.created_at = side
        .created_at_hint()
        .or_else(|| file_mtime_unix(torrent_path));

    if ins.complete {
        ins.state = "stopped".into();
        ins.finished_at = side
            .finished_at_hint()
            .or(ins.created_at)
            .or_else(|| Some(TorrentInsert::now_unix()));
    } else if let Some(fin) = side.finished_at_hint() {
        ins.finished_at = Some(fin);
    }

    if opts.start_after {
        ins.want_start = true;
        ins.state = "started".into();
    }
    // Stable announce key from session, or generate on insert.
    ins.tracker_key = side.key.filter(|&k| k != 0);

    let uploaded = ins.uploaded;
    let downloaded = ins.downloaded;

    match catalog.insert_torrent(&ins)? {
        InsertOutcome::Inserted { .. } => Ok(ImportOneResult {
            kind: ImportOneKind::Inserted,
            uploaded,
            downloaded,
        }),
        // Soft-deleted same infohash: insert_torrent already cleared deleted.
        // Refresh stats/priorities like a normal re-import, count as inserted
        // so the client shows up again.
        InsertOutcome::Restored { id } => {
            catalog.update_import_meta(
                id,
                ins.created_at,
                ins.finished_at,
                Some(ins.uploaded),
                Some(ins.downloaded),
            )?;
            if let Some(k) = side.key.filter(|&k| k != 0) {
                if catalog.tracker_key(id).unwrap_or(0) == 0 {
                    let _ = catalog.set_tracker_key(id, k);
                }
            } else {
                let _ = catalog.ensure_tracker_key(id);
            }
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
            // Re-import refreshes timestamps/stats (and never lowers totals).
            catalog.update_import_meta(
                id,
                ins.created_at,
                ins.finished_at,
                Some(ins.uploaded),
                Some(ins.downloaded),
            )?;
            // If catalog key is still zero, apply the session key.
            if let Some(k) = side.key.filter(|&k| k != 0) {
                if catalog.tracker_key(id).unwrap_or(0) == 0 {
                    let _ = catalog.set_tracker_key(id, k);
                }
            } else {
                let _ = catalog.ensure_tracker_key(id);
            }
            // File on/off from resume (0=off, ≥1=on).
            if !ins.file_priorities.is_empty() {
                catalog.set_file_priorities(id, &ins.file_priorities)?;
            }
            // Backfill original .torrent bytes if missing (or refresh).
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

/// `40 hex chars` + `.torrent` (uppercase or lowercase).
fn is_session_torrent_name(name: &str) -> bool {
    if name.len() != 48 || !name.ends_with(".torrent") {
        return false;
    }
    let hex = &name[..40];
    hex.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use seedchamp_engine::Metainfo;

    #[test]
    fn session_name_filter() {
        assert!(is_session_torrent_name(
            "0123456789ABCDEF0123456789ABCDEF01234567.torrent"
        ));
        assert!(!is_session_torrent_name("foo.torrent"));
        assert!(!is_session_torrent_name(
            "0123456789ABCDEF0123456789ABCDEF01234567.torrent.rtorrent"
        ));
    }

    #[test]
    fn strip_multi_file_directory_name() {
        assert_eq!(
            strip_trailing_torrent_name("/dl/MyTorrent", "MyTorrent"),
            "/dl"
        );
        assert_eq!(
            strip_trailing_torrent_name("/dl/other", "MyTorrent"),
            "/dl/other"
        );
    }

    #[test]
    fn dry_run_and_import_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        // Build minimal torrent file with fixed name matching infohash after parse —
        // session names don't have to match infohash for import; only pattern.
        let pieces = vec![0u8; 20];
        let mut info = Vec::new();
        info.extend_from_slice(b"d6:lengthi1e4:name4:test12:piece lengthi16384e6:pieces20:");
        info.extend_from_slice(&pieces);
        info.extend_from_slice(b"e");
        let mut root = Vec::new();
        root.extend_from_slice(b"d8:announce8:http://x4:info");
        root.extend_from_slice(&info);
        root.extend_from_slice(b"e");

        let m = Metainfo::parse_bytes(&root).unwrap();
        let name = format!("{}.torrent", m.infohash_hex().to_uppercase());
        let tpath = dir.path().join(&name);
        std::fs::write(&tpath, &root).unwrap();

        // resume: complete bitfield as integer
        let resume = b"d8:bitfieldi1e8:uploadedi100e10:downloadedi0ee";
        std::fs::write(dir.path().join(format!("{name}.libtorrent_resume")), resume).unwrap();

        // rtorrent directory + timestamps (created_at / finished_at)
        let rtorrent = b"d9:directory3:/dl17:timestamp.startedi1700000000e18:timestamp.finishedi1700000500e14:total_uploadedi77e16:total_downloadedi0ee";
        std::fs::write(dir.path().join(format!("{name}.rtorrent")), rtorrent).unwrap();

        let report = import_session(dir.path(), &dir.path().join("x.sqlite"), true).unwrap();
        assert_eq!(report.scanned, 1);
        assert_eq!(report.imported, 1);

        let db = dir.path().join("cat.sqlite");
        let report = import_session(dir.path(), &db, false).unwrap();
        assert_eq!(report.imported, 1);
        assert_eq!(report.errors.len(), 0);

        let cat = Catalog::open(&db).unwrap();
        let list = cat.list_torrents().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "test");
        assert!(list[0].complete);
        assert_eq!(list[0].data_root.as_deref(), Some("/dl"));
        assert_eq!(list[0].created_at, 1_700_000_000);
        // resume uploaded=100, rtorrent total_uploaded=77 → max = 100
        assert_eq!(list[0].uploaded, 100);
        assert_eq!(report.uploaded_bytes, 100);
        assert_eq!(report.with_transfer_stats, 1);
        let blob = cat.get_metainfo_blob(list[0].id).unwrap();
        assert!(blob.is_some());
        assert_eq!(blob.unwrap(), root);

        // second import: exists + refreshes metadata
        let report2 = import_session(dir.path(), &db, false).unwrap();
        assert_eq!(report2.skipped, 1);
        assert_eq!(report2.updated, 1);
        assert_eq!(report2.imported, 0);
        assert_eq!(report2.uploaded_bytes, 100);

        let cat = Catalog::open(&db).unwrap();
        let list = cat.list_torrents().unwrap();
        assert_eq!(list[0].created_at, 1_700_000_000);
        assert_eq!(list[0].uploaded, 100);
    }
}
