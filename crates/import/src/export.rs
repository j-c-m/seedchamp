//! Export seedchamp catalog torrents into rtorrent / Transmission session trees.

use std::fs;
use std::path::Path;

use seedchamp_engine::bencode::{self, Value};
use seedchamp_engine::{Catalog, Metainfo, Result};

/// Summary of an export run.
#[derive(Debug, Default)]
pub struct ExportReport {
    pub candidates: u32,
    pub written: u32,
    pub skipped: u32,
    pub errors: Vec<String>,
}

/// Catalog fields needed to write one session torrent.
struct ExportTorrent {
    infohash_hex: String,
    name: String,
    is_multi_file: bool,
    data_root: String,
    complete: bool,
    /// Piece bitfield bytes when incomplete; ignored when complete.
    bitfield: Option<Vec<u8>>,
    piece_count: u32,
    uploaded: u64,
    downloaded: u64,
    created_at: i64,
    finished_at: Option<i64>,
    want_start: bool,
    file_priorities: Vec<i32>,
    tracker_key: u32,
    metainfo_blob: Vec<u8>,
}

/// Export entire catalog to a flat rtorrent session directory.
pub fn export_rtorrent_all(
    db_path: &Path,
    session_dir: &Path,
    dry_run: bool,
) -> Result<ExportReport> {
    export_all(db_path, dry_run, |t, report| {
        if dry_run {
            report.written += 1;
            return Ok(());
        }
        fs::create_dir_all(session_dir)
            .map_err(|e| seedchamp_engine::Error::Path(session_dir.to_path_buf(), e.to_string()))?;
        write_rtorrent_one(session_dir, t)?;
        report.written += 1;
        Ok(())
    })
}

/// Export entire catalog to a Transmission config root (`torrents/` + `resume/`).
pub fn export_transmission_all(
    db_path: &Path,
    session_dir: &Path,
    dry_run: bool,
) -> Result<ExportReport> {
    export_all(db_path, dry_run, |t, report| {
        if dry_run {
            report.written += 1;
            return Ok(());
        }
        let torrents = session_dir.join("torrents");
        let resume = session_dir.join("resume");
        fs::create_dir_all(&torrents)
            .map_err(|e| seedchamp_engine::Error::Path(torrents.clone(), e.to_string()))?;
        fs::create_dir_all(&resume)
            .map_err(|e| seedchamp_engine::Error::Path(resume.clone(), e.to_string()))?;
        write_transmission_one(session_dir, t)?;
        report.written += 1;
        Ok(())
    })
}

fn export_all(
    db_path: &Path,
    _dry_run: bool,
    mut write_one: impl FnMut(&ExportTorrent, &mut ExportReport) -> Result<()>,
) -> Result<ExportReport> {
    let cat = Catalog::open(db_path)?;
    let rows = cat.list_torrents()?;
    let mut report = ExportReport {
        candidates: rows.len() as u32,
        ..Default::default()
    };

    for row in rows {
        match load_export_torrent(&cat, row.id) {
            Ok(None) => {
                report.skipped += 1;
                report.errors.push(format!("#{}: no metainfo blob", row.id));
            }
            Ok(Some(t)) => {
                if let Err(e) = write_one(&t, &mut report) {
                    report.skipped += 1;
                    report.errors.push(format!("#{}: {e}", row.id));
                }
            }
            Err(e) => {
                report.skipped += 1;
                report.errors.push(format!("#{}: {e}", row.id));
            }
        }
    }
    Ok(report)
}

fn load_export_torrent(cat: &Catalog, id: i64) -> Result<Option<ExportTorrent>> {
    let Some(blob) = cat.get_metainfo_blob(id)? else {
        return Ok(None);
    };
    let meta = Metainfo::parse_bytes(&blob)?;
    let detail = cat.get_torrent_detail(id)?;
    let (complete, bits, _have) = cat.load_bitfield_bytes(id)?;
    let files = cat.list_files(id)?;
    let data_root = cat
        .get_data_root(id)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| detail.list.data_root.clone().unwrap_or_default());
    let tracker_key = cat.tracker_key(id).unwrap_or(0);
    // Complete or empty → no piece map (resume writers use all/none flags).
    let bitfield = if complete || bits.iter().all(|&b| b == 0) {
        None
    } else {
        Some(bits)
    };

    Ok(Some(ExportTorrent {
        // rtorrent session filenames use uppercase hex (hash_string_to_hex).
        // Transmission accepts either; we keep lowercase there via write path.
        infohash_hex: meta.infohash_hex().to_ascii_lowercase(),
        name: meta.name.clone(),
        is_multi_file: meta.is_multi_file,
        data_root,
        complete,
        bitfield,
        piece_count: meta.piece_count,
        uploaded: detail.list.uploaded,
        downloaded: detail.list.downloaded,
        created_at: detail.list.created_at,
        finished_at: detail.finished_at,
        want_start: detail.list.want_start,
        file_priorities: files.iter().map(|f| f.priority).collect(),
        tracker_key,
        metainfo_blob: blob,
    }))
}

fn write_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .map_err(|e| seedchamp_engine::Error::Path(parent.to_path_buf(), e.to_string()))?;
        }
    }
    fs::write(path, bytes)
        .map_err(|e| seedchamp_engine::Error::Path(path.to_path_buf(), e.to_string()))?;
    Ok(())
}

fn rtorrent_directory(t: &ExportTorrent) -> String {
    if t.is_multi_file {
        let base = Path::new(&t.data_root);
        if base.file_name().and_then(|s| s.to_str()) == Some(t.name.as_str()) {
            t.data_root.clone()
        } else {
            base.join(&t.name).display().to_string()
        }
    } else {
        t.data_root.clone()
    }
}

fn write_rtorrent_one(session_dir: &Path, t: &ExportTorrent) -> Result<()> {
    // rtorrent names session files with uppercase hex (see hash_string_to_hex).
    let ih = t.infohash_hex.to_ascii_uppercase();
    let torrent_path = session_dir.join(format!("{ih}.torrent"));
    write_bytes(&torrent_path, &t.metainfo_blob)?;

    // .rtorrent sidecar — `state` is d.state (1 started / 0 stopped); session load
    // puts the torrent on the started or stopped view from this flag.
    let mut side: Vec<(String, Value)> = vec![
        (
            "directory".into(),
            Value::Bytes(rtorrent_directory(t).into_bytes()),
        ),
        ("total_uploaded".into(), Value::Int(t.uploaded as i64)),
        ("total_downloaded".into(), Value::Int(t.downloaded as i64)),
        ("state".into(), Value::Int(if t.want_start { 1 } else { 0 })),
    ];
    if t.created_at > 0 {
        side.push(("timestamp.started".into(), Value::Int(t.created_at)));
    }
    if let Some(fin) = t.finished_at.filter(|&x| x > 0) {
        side.push(("timestamp.finished".into(), Value::Int(fin)));
    }
    if t.tracker_key != 0 {
        side.push(("key".into(), Value::Int(t.tracker_key as i64)));
    }
    let side_enc = bencode::encode(&bencode::dict_from_str_keys(side));
    write_bytes(
        &session_dir.join(format!("{ih}.torrent.rtorrent")),
        &side_enc,
    )?;

    // .libtorrent_resume
    let mut resume_pairs: Vec<(String, Value)> = vec![
        ("uploaded".into(), Value::Int(t.uploaded as i64)),
        ("downloaded".into(), Value::Int(t.downloaded as i64)),
    ];
    if t.complete {
        resume_pairs.push(("bitfield".into(), Value::Int(t.piece_count as i64)));
    } else if let Some(ref bf) = t.bitfield {
        resume_pairs.push(("bitfield".into(), Value::Bytes(bf.clone())));
    } else {
        resume_pairs.push(("bitfield".into(), Value::Int(0)));
    }
    if !t.file_priorities.is_empty() {
        let files: Vec<Value> = t
            .file_priorities
            .iter()
            .map(|&p| bencode::dict_from_str_keys([("priority".into(), Value::Int(p as i64))]))
            .collect();
        resume_pairs.push(("files".into(), Value::List(files)));
    }
    let resume_enc = bencode::encode(&bencode::dict_from_str_keys(resume_pairs));
    write_bytes(
        &session_dir.join(format!("{ih}.torrent.libtorrent_resume")),
        &resume_enc,
    )?;
    Ok(())
}

fn write_transmission_one(session_dir: &Path, t: &ExportTorrent) -> Result<()> {
    let ih = &t.infohash_hex;
    let torrent_path = session_dir.join("torrents").join(format!("{ih}.torrent"));
    write_bytes(&torrent_path, &t.metainfo_blob)?;

    let blocks = if t.complete {
        Value::Bytes(b"all".to_vec())
    } else {
        Value::Bytes(b"none".to_vec())
    };
    let pieces = if t.complete {
        Value::Bytes(b"all".to_vec())
    } else {
        Value::Bytes(b"none".to_vec())
    };
    let progress =
        bencode::dict_from_str_keys([("blocks".into(), blocks), ("pieces".into(), pieces)]);

    let dnd: Vec<Value> = t
        .file_priorities
        .iter()
        .map(|&p| Value::Int(if p <= 0 { 1 } else { 0 }))
        .collect();
    // If no priorities stored, single-file default wanted.
    let dnd = if dnd.is_empty() {
        Value::List(vec![Value::Int(0)])
    } else {
        Value::List(dnd)
    };

    let priority: Vec<Value> = if t.file_priorities.is_empty() {
        vec![Value::Int(0)]
    } else {
        t.file_priorities
            .iter()
            .map(|_| Value::Int(0)) // TR normal priority
            .collect()
    };

    let mut pairs: Vec<(String, Value)> = vec![
        (
            "destination".into(),
            Value::Bytes(t.data_root.clone().into_bytes()),
        ),
        ("downloaded".into(), Value::Int(t.downloaded as i64)),
        ("uploaded".into(), Value::Int(t.uploaded as i64)),
        (
            "paused".into(),
            Value::Int(if t.want_start { 0 } else { 1 }),
        ),
        (
            "added-date".into(),
            Value::Int(if t.created_at > 0 { t.created_at } else { 0 }),
        ),
        (
            "done-date".into(),
            Value::Int(t.finished_at.filter(|&x| x > 0).unwrap_or(0)),
        ),
        ("progress".into(), progress),
        ("dnd".into(), dnd),
        ("priority".into(), Value::List(priority)),
        ("name".into(), Value::Bytes(t.name.clone().into_bytes())),
    ];

    let enc = bencode::encode(&bencode::dict_from_str_keys(pairs.drain(..)));
    write_bytes(
        &session_dir.join("resume").join(format!("{ih}.resume")),
        &enc,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use seedchamp_engine::{Catalog, TorrentInsert};
    use std::fs;

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

    /// Multi-file: name=`pack`, two 1-byte files (one piece).
    fn multi_file_torrent_bytes() -> Vec<u8> {
        let pieces = vec![0u8; 20];
        let mut info = Vec::new();
        info.extend_from_slice(b"d4:name4:pack5:filesl");
        info.extend_from_slice(b"d6:lengthi1e4:pathl1:a1:xee");
        info.extend_from_slice(b"d6:lengthi1e4:pathl1:bee");
        info.extend_from_slice(b"e12:piece lengthi16384e6:pieces20:");
        info.extend_from_slice(&pieces);
        info.extend_from_slice(b"e");
        let mut root = Vec::new();
        root.extend_from_slice(b"d8:announce8:http://x4:info");
        root.extend_from_slice(&info);
        root.extend_from_slice(b"e");
        root
    }

    fn seed_catalog(db: &Path) -> i64 {
        seed_catalog_want(db, false)
    }

    fn seed_catalog_want(db: &Path, want_start: bool) -> i64 {
        seed_blob(db, &sample_torrent_bytes(), "/dl", want_start)
    }

    fn seed_blob(db: &Path, blob: &[u8], data_root: &str, want_start: bool) -> i64 {
        let meta = Metainfo::parse_bytes(blob).unwrap();
        let mut ins = TorrentInsert::from_metainfo(meta, data_root);
        ins.metainfo_blob = Some(blob.to_vec());
        ins.complete = true;
        ins.have_count = 1;
        ins.uploaded = 99;
        ins.downloaded = 50;
        ins.created_at = Some(1_700_000_000);
        ins.finished_at = Some(1_700_000_500);
        ins.want_start = want_start;
        let mut cat = Catalog::open(db).unwrap();
        cat.insert_torrent(&ins).unwrap().id()
    }

    fn read_session_dict(sess: &Path, suffix: &str) -> bencode::Value {
        let path = fs::read_dir(sess)
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.ends_with(suffix))
            })
            .unwrap_or_else(|| panic!("missing *{suffix} under {sess:?}"));
        bencode::decode_full(&fs::read(&path).unwrap()).unwrap()
    }

    fn rtorrent_side_state(sess: &Path) -> i64 {
        match read_session_dict(sess, ".torrent.rtorrent").dict_get_int("state") {
            Some(s) => s,
            None => panic!("missing state in rtorrent sidecar"),
        }
    }

    #[test]
    fn rtorrent_export_import_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("c.sqlite");
        seed_catalog(&db);
        let sess = dir.path().join("rt");
        let rep = export_rtorrent_all(&db, &sess, false).unwrap();
        assert_eq!(rep.written, 1);
        assert_eq!(rep.errors.len(), 0);

        // rtorrent requires uppercase infohash filenames
        let names: Vec<_> = fs::read_dir(&sess)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert!(
            names.iter().any(|n| n.ends_with(".torrent")
                && n.len() == 48
                && n[..40]
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())),
            "expected uppercase session torrent, got {names:?}"
        );

        // second export overwrites
        let rep2 = export_rtorrent_all(&db, &sess, false).unwrap();
        assert_eq!(rep2.written, 1);

        let db2 = dir.path().join("c2.sqlite");
        let imp = crate::import_session(&sess, &db2, false).unwrap();
        assert_eq!(imp.imported, 1);
        let cat = Catalog::open(&db2).unwrap();
        let list = cat.list_torrents().unwrap();
        assert_eq!(list.len(), 1);
        assert!(list[0].complete);
        assert_eq!(list[0].uploaded, 99);
        assert_eq!(list[0].data_root.as_deref(), Some("/dl"));
    }

    #[test]
    fn transmission_export_import_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("c.sqlite");
        seed_catalog(&db);
        let sess = dir.path().join("tr");
        let rep = export_transmission_all(&db, &sess, false).unwrap();
        assert_eq!(rep.written, 1);
        assert!(sess.join("torrents").is_dir());
        assert!(sess.join("resume").is_dir());

        let db2 = dir.path().join("c2.sqlite");
        let imp = crate::import_transmission(&sess, &db2, false).unwrap();
        assert_eq!(imp.imported, 1);
        let cat = Catalog::open(&db2).unwrap();
        let list = cat.list_torrents().unwrap();
        assert_eq!(list.len(), 1);
        assert!(list[0].complete);
        assert_eq!(list[0].uploaded, 99);
        assert_eq!(list[0].data_root.as_deref(), Some("/dl"));
    }

    #[test]
    fn rtorrent_export_maps_want_start_to_state() {
        let dir = tempfile::tempdir().unwrap();
        let db_off = dir.path().join("off.sqlite");
        seed_catalog_want(&db_off, false);
        let sess_off = dir.path().join("rt-off");
        export_rtorrent_all(&db_off, &sess_off, false).unwrap();
        assert_eq!(rtorrent_side_state(&sess_off), 0);

        let db_on = dir.path().join("on.sqlite");
        seed_catalog_want(&db_on, true);
        let sess_on = dir.path().join("rt-on");
        export_rtorrent_all(&db_on, &sess_on, false).unwrap();
        assert_eq!(rtorrent_side_state(&sess_on), 1);
    }

    #[test]
    fn multi_file_directory_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let blob = multi_file_torrent_bytes();
        let meta = Metainfo::parse_bytes(&blob).unwrap();
        assert!(meta.is_multi_file);
        assert_eq!(meta.name, "pack");

        // Catalog stores parent only (BEP 3 paths include pack/…).
        let db = dir.path().join("c.sqlite");
        seed_blob(&db, &blob, "/dl", false);

        // rtorrent session `directory` must include the torrent name.
        let rt = dir.path().join("rt");
        export_rtorrent_all(&db, &rt, false).unwrap();
        let side = read_session_dict(&rt, ".torrent.rtorrent");
        assert_eq!(side.dict_get_str("directory"), Some("/dl/pack"));

        // Re-import strips the name so catalog data_root is the parent again.
        let db_rt = dir.path().join("from-rt.sqlite");
        let imp = crate::import_session(&rt, &db_rt, false).unwrap();
        assert_eq!(imp.imported, 1);
        let list = Catalog::open(&db_rt).unwrap().list_torrents().unwrap();
        assert_eq!(list[0].data_root.as_deref(), Some("/dl"));

        // If data_root already ends with the name, do not double-nest.
        let db_named = dir.path().join("named.sqlite");
        seed_blob(&db_named, &blob, "/dl/pack", false);
        let rt2 = dir.path().join("rt2");
        export_rtorrent_all(&db_named, &rt2, false).unwrap();
        let side2 = read_session_dict(&rt2, ".torrent.rtorrent");
        assert_eq!(side2.dict_get_str("directory"), Some("/dl/pack"));

        // Transmission destination stays the catalog parent (no name append).
        let tr = dir.path().join("tr");
        export_transmission_all(&db, &tr, false).unwrap();
        let resume = fs::read_dir(tr.join("resume"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let dest = bencode::decode_full(&fs::read(&resume).unwrap()).unwrap();
        assert_eq!(dest.dict_get_str("destination"), Some("/dl"));

        let db_tr = dir.path().join("from-tr.sqlite");
        let imp_tr = crate::import_transmission(&tr, &db_tr, false).unwrap();
        assert_eq!(imp_tr.imported, 1);
        let list_tr = Catalog::open(&db_tr).unwrap().list_torrents().unwrap();
        assert_eq!(list_tr[0].data_root.as_deref(), Some("/dl"));
    }

    #[test]
    fn missing_blob_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("c.sqlite");
        // insert without blob
        let meta = Metainfo::parse_bytes(&sample_torrent_bytes()).unwrap();
        let ins = TorrentInsert::from_metainfo(meta, "/dl");
        let mut cat = Catalog::open(&db).unwrap();
        cat.insert_torrent(&ins).unwrap();
        drop(cat);

        let sess = dir.path().join("rt");
        let rep = export_rtorrent_all(&db, &sess, false).unwrap();
        assert_eq!(rep.candidates, 1);
        assert_eq!(rep.written, 0);
        assert_eq!(rep.skipped, 1);
        assert!(!rep.errors.is_empty());
    }

    #[test]
    fn dry_run_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("c.sqlite");
        seed_catalog(&db);
        let sess = dir.path().join("rt");
        let rep = export_rtorrent_all(&db, &sess, true).unwrap();
        assert_eq!(rep.written, 1);
        assert!(!sess.exists() || fs::read_dir(&sess).map(|d| d.count()).unwrap_or(0) == 0);
    }
}
