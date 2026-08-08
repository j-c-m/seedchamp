//! Directory watchers — rtorrent `schedule2 = watch_*` equivalent.
//!
//! Polls configured directories for `*.torrent` files, adds them to the catalog
//! with optional date-stamped data roots, optional auto-start, and optional
//! delete-tied (remove the drop-in `.torrent` after load).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::catalog::Catalog;
use crate::config::{WatchConfig, WatchDirConfig};
use crate::error::{Error, Result};
use crate::library::{add_torrent_bytes, load_torrent_bytes, AddOptions, AddReport};
use crate::metainfo::Metainfo;

/// Result of processing one watched file.
#[derive(Debug, Clone)]
pub struct WatchLoadEvent {
    pub watch_name: String,
    pub torrent_path: PathBuf,
    pub report: AddReport,
    /// True when the watch dir requested auto-start.
    pub start: bool,
    pub deleted_after_import: bool,
}

/// Callback when a torrent is loaded (and optionally should be started).
pub type WatchCallback = Arc<dyn Fn(WatchLoadEvent) + Send + Sync>;

/// Background watch loop handle.
pub struct WatchHandle {
    stop: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
}

impl WatchHandle {
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for WatchHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// Spawn a background thread that scans `watch` dirs until dropped/stopped.
///
/// `default_data_root` / `torrent_dir` come from `paths.*` when a dir omits them.
/// `on_load` is invoked after each successful load (use to `start_torrent` when
/// `event.start` is true).
pub fn spawn_watcher(
    db: PathBuf,
    watch: WatchConfig,
    default_data_root: PathBuf,
    torrent_dir: PathBuf,
    leech_cache: PathBuf,
    leech_cache_size: u64,
    on_load: Option<WatchCallback>,
) -> Result<WatchHandle> {
    if !watch.enabled {
        return Err(Error::Msg("watch disabled".into()));
    }
    let active: Vec<WatchDirConfig> = watch
        .dirs
        .into_iter()
        .filter(|d| d.enabled && !d.path.as_os_str().is_empty())
        .collect();
    if active.is_empty() {
        return Err(Error::Msg("watch enabled but no dirs configured".into()));
    }
    let interval = Duration::from_secs(watch.interval_secs.max(1));
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let join = thread::Builder::new()
        .name("seedchamp-watch".into())
        .spawn(move || {
            tracing::info!(
                dirs = active.len(),
                interval_secs = interval.as_secs(),
                "watch loop started"
            );
            while !stop2.load(Ordering::SeqCst) {
                for dir in &active {
                    if stop2.load(Ordering::SeqCst) {
                        break;
                    }
                    // Always readdir + try each *.torrent (already-in-catalog is a no-op).
                    if let Err(e) = scan_dir(
                        &db,
                        dir,
                        &default_data_root,
                        &torrent_dir,
                        &leech_cache,
                        leech_cache_size,
                        on_load.as_ref(),
                    ) {
                        tracing::warn!(
                            watch = %dir_label(dir),
                            error = %e,
                            "watch scan"
                        );
                    }
                }
                // Interruptible sleep (1s steps — no need for 200ms when interval ≥ 1s).
                let step = Duration::from_secs(1).min(interval);
                let mut left = interval;
                while left > Duration::ZERO && !stop2.load(Ordering::SeqCst) {
                    let slice = step.min(left);
                    thread::sleep(slice);
                    left = left.saturating_sub(slice);
                }
            }
            tracing::info!("watch loop stopped");
        })
        .map_err(|e| Error::Msg(format!("spawn watch: {e}")))?;
    Ok(WatchHandle {
        stop,
        join: Some(join),
    })
}

/// One-shot scan of all active dirs (tests / `seedchamp watch --once`).
pub fn poll_watch_once(
    db: &Path,
    watch: &WatchConfig,
    default_data_root: &Path,
    torrent_dir: &Path,
    leech_cache: &Path,
    leech_cache_size: u64,
    on_load: Option<&WatchCallback>,
) -> Result<Vec<WatchLoadEvent>> {
    let mut out = Vec::new();
    for dir in watch
        .dirs
        .iter()
        .filter(|d| d.enabled && !d.path.as_os_str().is_empty())
    {
        let events = scan_dir_collect(
            db,
            dir,
            default_data_root,
            torrent_dir,
            leech_cache,
            leech_cache_size,
            on_load,
        )?;
        out.extend(events);
    }
    Ok(out)
}

fn scan_dir(
    db: &Path,
    dir: &WatchDirConfig,
    default_data_root: &Path,
    torrent_dir: &Path,
    leech_cache: &Path,
    leech_cache_size: u64,
    on_load: Option<&WatchCallback>,
) -> Result<()> {
    let _ = scan_dir_collect(
        db,
        dir,
        default_data_root,
        torrent_dir,
        leech_cache,
        leech_cache_size,
        on_load,
    )?;
    Ok(())
}

fn scan_dir_collect(
    db: &Path,
    dir: &WatchDirConfig,
    default_data_root: &Path,
    torrent_dir: &Path,
    leech_cache: &Path,
    leech_cache_size: u64,
    on_load: Option<&WatchCallback>,
) -> Result<Vec<WatchLoadEvent>> {
    let path = &dir.path;
    if !path.is_dir() {
        // Soft: dir may not exist yet.
        tracing::debug!(path = %path.display(), "watch path missing");
        return Ok(Vec::new());
    }
    let mut files: Vec<PathBuf> = fs::read_dir(path)
        .map_err(|e| Error::Path(path.clone(), e.to_string()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file() && is_torrent_file(p))
        .collect();
    files.sort();
    let mut events = Vec::new();
    for file in files {
        match load_one(
            db,
            dir,
            &file,
            default_data_root,
            torrent_dir,
            leech_cache,
            leech_cache_size,
        ) {
            Ok(Some(ev)) => {
                if let Some(cb) = on_load {
                    cb(ev.clone());
                }
                events.push(ev);
            }
            Ok(None) => {}
            Err(e) => {
                // Bad .torrent: skip and keep scanning the rest of the dir.
                tracing::warn!(
                    watch = %dir_label(dir),
                    file = %file.display(),
                    error = %e,
                    "watch load failed"
                );
            }
        }
    }
    Ok(events)
}

fn load_one(
    db: &Path,
    dir: &WatchDirConfig,
    torrent_path: &Path,
    default_data_root: &Path,
    torrent_dir: &Path,
    leech_cache: &Path,
    leech_cache_size: u64,
) -> Result<Option<WatchLoadEvent>> {
    // Parse first so `{torrent_name}` / `{ih8}` can expand before mkdir.
    let source = torrent_path.display().to_string();
    let (bytes, label) = load_torrent_bytes(&source)?;
    let meta = Metainfo::parse_bytes(&bytes)?;
    let wname = dir_label(dir);
    let ctx = DlPathContext::from_metainfo(&wname, &meta);
    let data_root = resolve_dl_path(dir, default_data_root, &ctx);
    fs::create_dir_all(&data_root).map_err(|e| Error::Path(data_root.clone(), e.to_string()))?;
    let data_root_disp = data_root.display().to_string();

    let save_dir = if dir.save_torrent {
        Some(torrent_dir.to_path_buf())
    } else {
        None
    };
    let opts = AddOptions {
        data_root,
        leech_cache: leech_cache.to_path_buf(),
        leech_cache_size,
        start: dir.start,
        save_torrent_dir: save_dir,
    };

    let mut cat = Catalog::open(db)?;
    let report = add_torrent_bytes(&mut cat, &bytes, &label, &opts)?;

    let should_delete = if report.already_existed {
        dir.delete_after_import && dir.delete_after_import_if_exists
    } else {
        dir.delete_after_import
    };
    let mut deleted_after_import = false;
    if should_delete {
        match fs::remove_file(torrent_path) {
            Ok(()) => deleted_after_import = true,
            Err(e) => tracing::warn!(
                file = %torrent_path.display(),
                error = %e,
                "delete_after_import failed"
            ),
        }
    }

    if report.already_existed && !dir.start {
        // Nothing new for the UI unless we deleted the file.
        if !deleted_after_import {
            return Ok(None);
        }
    }

    tracing::info!(
        watch = %wname,
        id = report.id,
        name = %report.name,
        existed = report.already_existed,
        start = dir.start,
        deleted = deleted_after_import,
        data = %data_root_disp,
        "watch loaded"
    );

    Ok(Some(WatchLoadEvent {
        watch_name: wname,
        torrent_path: torrent_path.to_path_buf(),
        report,
        start: dir.start,
        deleted_after_import,
    }))
}

fn dir_label(dir: &WatchDirConfig) -> String {
    if let Some(n) = &dir.name {
        if !n.is_empty() {
            return n.clone();
        }
    }
    dir.path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| dir.path.display().to_string())
}

/// Values for `dl_path` placeholders that are not pure calendar tokens.
#[derive(Debug, Clone, Default)]
pub struct DlPathContext {
    /// Watch dir label (`name` or path basename), already sanitized for paths.
    pub watch_name: String,
    /// Sanitized torrent name (empty if unknown).
    pub torrent_name: String,
    /// First 8 hex chars of infohash (empty if unknown).
    pub ih8: String,
}

impl DlPathContext {
    pub fn watch_only(watch_name: &str) -> Self {
        Self {
            watch_name: sanitize_path_component(watch_name),
            torrent_name: String::new(),
            ih8: String::new(),
        }
    }

    pub fn from_metainfo(watch_name: &str, meta: &Metainfo) -> Self {
        let hex = meta.infohash_hex();
        Self {
            watch_name: sanitize_path_component(watch_name),
            torrent_name: sanitize_path_component(&meta.name),
            ih8: hex.chars().take(8).collect(),
        }
    }
}

/// Resolve download path from `dl_path` template (or `default_data_root`).
///
/// Placeholders (local time unless noted):
/// - `{date}` → `YYYY-MM-DD`, `{YYYY}`, `{YY}`, `{MM}`, `{DD}`
/// - `{watch_name}` — watch label (sanitized)
/// - `{torrent_name}` — torrent name (sanitized); empty if not in `ctx`
/// - `{ih8}` — first 8 hex of infohash; empty if not in `ctx`
pub fn resolve_dl_path(
    dir: &WatchDirConfig,
    default_data_root: &Path,
    ctx: &DlPathContext,
) -> PathBuf {
    let template = dir
        .dl_path
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| default_data_root.display().to_string());
    PathBuf::from(expand_dl_path_template(&template, ctx))
}

/// Expand `dl_path` placeholders. See [`resolve_dl_path`].
pub fn expand_dl_path_template(template: &str, ctx: &DlPathContext) -> String {
    let (y, m, d) = local_ymd();
    let date = format!("{y:04}-{m:02}-{d:02}");
    let yy = format!("{:02}", y.rem_euclid(100));
    template
        .replace("{date}", &date)
        .replace("{YYYY}", &format!("{y:04}"))
        .replace("{yyyy}", &format!("{y:04}"))
        .replace("{YY}", &yy)
        .replace("{yy}", &yy)
        .replace("{MM}", &format!("{m:02}"))
        .replace("{DD}", &format!("{d:02}"))
        .replace("{watch_name}", &ctx.watch_name)
        .replace("{torrent_name}", &ctx.torrent_name)
        .replace("{ih8}", &ctx.ih8)
}

/// Make a single path component safe (no separators / reserved chars).
pub fn sanitize_path_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_us = false;
    for ch in s.chars() {
        let bad = matches!(
            ch,
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0'
        ) || ch.is_control();
        if bad || ch == ' ' {
            if !prev_us && !out.is_empty() {
                out.push('_');
                prev_us = true;
            }
        } else {
            out.push(ch);
            prev_us = false;
        }
    }
    let out = out
        .trim_matches(|c| c == '.' || c == '_' || c == ' ')
        .to_string();
    if out.is_empty() {
        "unnamed".into()
    } else if out.len() > 200 {
        out.chars().take(200).collect()
    } else {
        out
    }
}

/// `YYYY-MM-DD` in **local** time (UTC if local offset cannot be determined).
pub fn date_stamp() -> String {
    let (y, m, d) = local_ymd();
    format!("{y:04}-{m:02}-{d:02}")
}

fn local_ymd() -> (i32, u32, u32) {
    match time::OffsetDateTime::now_local() {
        Ok(t) => (t.year(), u32::from(t.month() as u8), u32::from(t.day())),
        Err(_) => utc_ymd(),
    }
}

fn utc_ymd() -> (i32, u32, u32) {
    let days = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() / 86_400)
        .unwrap_or(0);
    civil_from_days(days as i64)
}

/// Howard Hinnant `civil_from_days` (days since Unix epoch → y/m/d).
fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m as u32, d as u32)
}

fn is_torrent_file(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("torrent"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Catalog;
    use crate::config::WatchDirConfig;
    use std::io::Write;

    fn sample_torrent() -> Vec<u8> {
        let mut pieces = vec![0u8; 20];
        pieces[0] = 0xbb;
        let mut info = Vec::new();
        info.extend_from_slice(b"d6:lengthi1e4:name5:wtest12:piece lengthi16384e6:pieces20:");
        info.extend_from_slice(&pieces);
        info.extend_from_slice(b"e");
        let mut root = Vec::new();
        root.extend_from_slice(b"d8:announce8:http://x4:info");
        root.extend_from_slice(&info);
        root.extend_from_slice(b"e");
        root
    }

    #[test]
    fn date_stamp_format() {
        let s = date_stamp();
        assert_eq!(s.len(), 10);
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[7..8], "-");
    }

    #[test]
    fn sanitize_path_component_strips_separators() {
        assert_eq!(sanitize_path_component("a/b\\c:d"), "a_b_c_d");
        assert_eq!(sanitize_path_component("  ..  "), "unnamed");
        assert_eq!(sanitize_path_component("My Torrent!"), "My_Torrent!");
    }

    #[test]
    fn expand_template_tokens() {
        let ctx = DlPathContext {
            watch_name: "start".into(),
            torrent_name: "Cool_Show".into(),
            ih8: "deadbeef".into(),
        };
        let s = expand_dl_path_template("/data/{date}", &ctx);
        assert!(s.starts_with("/data/"));
        assert_eq!(s.len(), "/data/".len() + 10);
        let s2 = expand_dl_path_template("/dl/{watch_name}/{date}/{torrent_name}-{ih8}", &ctx);
        assert!(s2.contains("/start/"));
        assert!(s2.ends_with("Cool_Show-deadbeef") || s2.contains("Cool_Show-deadbeef"));
        assert!(!s2.contains('{'));
        let s3 = expand_dl_path_template("/dl/{YY}", &ctx);
        assert_eq!(s3.len(), "/dl/".len() + 2);
    }

    #[test]
    fn resolve_dl_path_template() {
        let dir = WatchDirConfig {
            dl_path: Some("/data/{date}".into()),
            ..Default::default()
        };
        let p = resolve_dl_path(&dir, Path::new("/default"), &DlPathContext::default());
        assert!(p.starts_with("/data/"));
        assert_eq!(p.components().count(), 3); // / data YYYY-MM-DD
    }

    #[test]
    fn load_and_delete_after_import() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("c.sqlite");
        let watch_dir = tmp.path().join("watch");
        let data = tmp.path().join("data");
        fs::create_dir_all(&watch_dir).unwrap();
        let tor = watch_dir.join("a.torrent");
        fs::File::create(&tor)
            .unwrap()
            .write_all(&sample_torrent())
            .unwrap();

        let mut wdir = WatchDirConfig::default();
        wdir.path = watch_dir.clone();
        wdir.dl_path = Some(data.display().to_string());
        wdir.start = true;
        wdir.delete_after_import = true;

        let cfg = WatchConfig {
            enabled: true,
            interval_secs: 1,
            dirs: vec![wdir],
        };
        let events = poll_watch_once(&db, &cfg, &data, tmp.path(), Path::new(""), 0, None).unwrap();
        assert_eq!(events.len(), 1);
        assert!(
            !tor.exists(),
            "delete_after_import should remove drop-in torrent"
        );
        assert!(events[0].start);
        assert!(events[0].deleted_after_import);

        let cat = Catalog::open(&db).unwrap();
        let rows = cat.list_torrents().unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].want_start);
    }
}
