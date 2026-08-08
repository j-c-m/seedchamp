//! Add a normal `.torrent` from disk path or HTTP(S) URL into the catalog.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::catalog::{Catalog, InsertOutcome, TorrentInsert};
use crate::error::{Error, Result};
use crate::metainfo::Metainfo;

#[derive(Debug, Clone)]
pub struct AddOptions {
    /// Permanent library directory for payload (home root).
    pub data_root: PathBuf,
    /// Optional SSD ingress (`paths.leech_cache`); empty = disabled.
    pub leech_cache: PathBuf,
    /// Soft max committed bytes under leech_cache (`0` = no soft cap).
    pub leech_cache_size: u64,
    /// Mark want_start / state=started after insert.
    pub start: bool,
    /// Optional directory to save a copy of the .torrent (created if needed).
    pub save_torrent_dir: Option<PathBuf>,
}

impl Default for AddOptions {
    fn default() -> Self {
        Self {
            data_root: PathBuf::from("."),
            leech_cache: PathBuf::new(),
            leech_cache_size: 0,
            start: false,
            save_torrent_dir: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AddReport {
    pub id: i64,
    pub infohash_hex: String,
    pub name: String,
    pub total_size: u64,
    pub piece_count: u32,
    pub trackers: usize,
    pub already_existed: bool,
    pub source: String,
    pub saved_torrent: Option<PathBuf>,
}

/// True if `source` looks like an HTTP(S) URL.
pub fn is_http_url(source: &str) -> bool {
    let s = source.trim();
    let lower = s.to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

/// Load raw .torrent bytes from a filesystem path or http(s) URL.
pub fn load_torrent_bytes(source: &str) -> Result<(Vec<u8>, String)> {
    let source = source.trim();
    if source.is_empty() {
        return Err(Error::Msg("empty torrent source".into()));
    }
    if is_http_url(source) {
        let bytes = fetch_url_bytes(source)?;
        Ok((bytes, source.to_string()))
    } else {
        let path = Path::new(source);
        let bytes = fs::read(path).map_err(|e| Error::Path(path.to_path_buf(), e.to_string()))?;
        Ok((bytes, path.display().to_string()))
    }
}

/// Max `.torrent` body when adding from HTTP(S) URL.
const MAX_TORRENT_BYTES: u64 = 32 * 1024 * 1024;

/// Sync entry for URL metainfo (CLI / TUI add thread / watch). Uses the shared
/// cyper client on a short-lived Compio runtime (or the current Compio runtime
/// when already inside one via nested block is avoided — always a private RT).
fn fetch_url_bytes(url: &str) -> Result<Vec<u8>> {
    let url = url.to_string();
    let rt = compio::runtime::Runtime::new()
        .map_err(|e| Error::Msg(format!("fetch torrent URL runtime: {e}")))?;
    rt.block_on(fetch_url_bytes_async(&url))
}

async fn fetch_url_bytes_async(url: &str) -> Result<Vec<u8>> {
    use crate::tracker::http::{http_get_bytes, tracker_user_agent};

    // Metainfo may be larger / slower than a tracker announce.
    let bytes = http_get_bytes(url, tracker_user_agent(), Duration::from_secs(60)).await?;
    if bytes.is_empty() {
        return Err(Error::Msg("torrent URL returned empty body".into()));
    }
    if bytes.len() as u64 > MAX_TORRENT_BYTES {
        return Err(Error::Msg(format!(
            "torrent URL body {} exceeds {MAX_TORRENT_BYTES} byte limit",
            bytes.len()
        )));
    }
    Ok(bytes)
}

/// Parse metainfo and insert into the catalog.
pub fn add_torrent_bytes(
    catalog: &mut Catalog,
    bytes: &[u8],
    source_label: &str,
    opts: &AddOptions,
) -> Result<AddReport> {
    let metainfo = Metainfo::parse_bytes(bytes)?;
    let infohash_hex = metainfo.infohash_hex();
    let name = metainfo.name.clone();
    let total_size = metainfo.total_size;
    let piece_count = metainfo.piece_count;
    let trackers = metainfo.trackers.len();

    let mut saved_torrent = None;
    if let Some(dir) = &opts.save_torrent_dir {
        fs::create_dir_all(dir).map_err(|e| Error::Path(dir.clone(), e.to_string()))?;
        let path = dir.join(format!("{infohash_hex}.torrent"));
        fs::write(&path, bytes).map_err(|e| Error::Path(path.clone(), e.to_string()))?;
        saved_torrent = Some(path);
    }

    let permanent = opts
        .data_root
        .canonicalize()
        .unwrap_or_else(|_| opts.data_root.clone());

    // Incomplete by default on add; complete=true only for rare import paths that set it later.
    let wanted = crate::library::wanted_bytes_from_metainfo(&metainfo, &[]);
    let reserved = if opts.leech_cache_size > 0 {
        catalog.leech_cache_reserved_bytes().unwrap_or_else(|e| {
            tracing::warn!(error = %e, "leech_cache reserved probe failed; treating as 0");
            0
        })
    } else {
        0
    };
    let place = crate::library::choose_placement(
        &permanent,
        &opts.leech_cache,
        opts.leech_cache_size,
        reserved,
        &infohash_hex,
        wanted,
        false,
    );
    if place.used_leech_cache {
        if let Err(e) = std::fs::create_dir_all(&place.data_root) {
            tracing::warn!(
                error = %e,
                path = %place.data_root.display(),
                "leech_cache create failed; falling back to permanent data_root"
            );
            // Fall through with permanent-only placement.
            let place = crate::library::Placement {
                data_root: permanent.clone(),
                home_root: None,
                used_leech_cache: false,
            };
            return insert_with_placement(
                catalog,
                metainfo,
                bytes,
                source_label,
                opts,
                place,
                infohash_hex,
                name,
                total_size,
                piece_count,
                trackers,
                saved_torrent,
            );
        }
        tracing::info!(
            stage = %place.data_root.display(),
            home = %permanent.display(),
            wanted,
            "add: staging incomplete torrent on leech_cache"
        );
    }

    insert_with_placement(
        catalog,
        metainfo,
        bytes,
        source_label,
        opts,
        place,
        infohash_hex,
        name,
        total_size,
        piece_count,
        trackers,
        saved_torrent,
    )
}

fn insert_with_placement(
    catalog: &mut Catalog,
    metainfo: Metainfo,
    bytes: &[u8],
    source_label: &str,
    opts: &AddOptions,
    place: crate::library::Placement,
    infohash_hex: String,
    name: String,
    total_size: u64,
    piece_count: u32,
    trackers: usize,
    saved_torrent: Option<PathBuf>,
) -> Result<AddReport> {
    let mut ins = TorrentInsert::from_metainfo(metainfo, place.data_root.display().to_string());
    ins.home_root = place.home_root.as_ref().map(|p| p.display().to_string());
    ins.metainfo_blob = Some(bytes.to_vec());
    ins.source_torrent = Some(
        saved_torrent
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| source_label.to_string()),
    );
    if opts.start {
        ins.want_start = true;
        ins.state = "started".into();
    }

    let outcome = catalog.insert_torrent(&ins)?;
    // Restored soft-delete counts as "new" for UI/watch (must show up again).
    let (id, already) = match outcome {
        InsertOutcome::Inserted { id } | InsertOutcome::Restored { id } => (id, false),
        InsertOutcome::Exists { id } => (id, true),
    };

    tracing::info!(
        id,
        torrent = %name,
        pieces = piece_count,
        size = total_size,
        existed = already,
        start = opts.start,
        source = %source_label,
        "add torrent"
    );

    Ok(AddReport {
        id,
        infohash_hex,
        name,
        total_size,
        piece_count,
        trackers,
        already_existed: already,
        source: source_label.to_string(),
        saved_torrent,
    })
}

/// Load from path or URL and insert.
pub fn add_torrent(catalog: &mut Catalog, source: &str, opts: &AddOptions) -> Result<AddReport> {
    match load_torrent_bytes(source) {
        Ok((bytes, label)) => add_torrent_bytes(catalog, &bytes, &label, opts).map_err(|e| {
            tracing::warn!(source = %source, error = %e, "add torrent failed");
            e
        }),
        Err(e) => {
            tracing::warn!(source = %source, error = %e, "add torrent failed");
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Catalog;

    fn sample_torrent() -> Vec<u8> {
        let mut pieces = vec![0u8; 20];
        pieces[0] = 0xaa;
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
    fn add_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let tor = dir.path().join("sample.torrent");
        fs::write(&tor, sample_torrent()).unwrap();
        let db = dir.path().join("c.sqlite");
        let mut cat = Catalog::open(&db).unwrap();
        let data = dir.path().join("data");
        fs::create_dir_all(&data).unwrap();
        let save = dir.path().join("torrents");
        let report = add_torrent(
            &mut cat,
            tor.to_str().unwrap(),
            &AddOptions {
                data_root: data,
                leech_cache: PathBuf::new(),
                leech_cache_size: 0,
                start: true,
                save_torrent_dir: Some(save.clone()),
            },
        )
        .unwrap();
        assert!(!report.already_existed);
        assert_eq!(report.name, "test");
        assert_eq!(report.piece_count, 1);
        assert!(report.saved_torrent.unwrap().is_file());
        assert!(save
            .join(format!("{}.torrent", report.infohash_hex))
            .is_file());

        // second add is exists
        let report2 = add_torrent(
            &mut cat,
            tor.to_str().unwrap(),
            &AddOptions {
                data_root: dir.path().join("data"),
                leech_cache: PathBuf::new(),
                leech_cache_size: 0,
                start: false,
                save_torrent_dir: None,
            },
        )
        .unwrap();
        assert!(report2.already_existed);
        assert_eq!(report2.id, report.id);

        // soft-delete then re-add → restored (shows as not already_existed)
        cat.set_want_start(report.id, false).unwrap();
        cat.mark_deleted(report.id).unwrap();
        assert!(cat.is_deleted(report.id).unwrap());
        assert!(cat.list_torrents().unwrap().is_empty());
        let report3 = add_torrent(
            &mut cat,
            tor.to_str().unwrap(),
            &AddOptions {
                data_root: dir.path().join("data"),
                leech_cache: PathBuf::new(),
                leech_cache_size: 0,
                start: true,
                save_torrent_dir: None,
            },
        )
        .unwrap();
        assert!(
            !report3.already_existed,
            "restored must surface like a new add"
        );
        assert_eq!(report3.id, report.id);
        assert!(!cat.is_deleted(report.id).unwrap());
        assert_eq!(cat.list_torrents().unwrap().len(), 1);
    }

    #[test]
    fn detects_urls() {
        assert!(is_http_url("https://example.com/a.torrent"));
        assert!(is_http_url("HTTP://x/y"));
        assert!(!is_http_url("/tmp/a.torrent"));
        assert!(!is_http_url("file.torrent"));
    }
}
