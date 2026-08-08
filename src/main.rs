//! seedchamp — greenfield BitTorrent seedbox TUI.
//!
//! Config: `$XDG_CONFIG_HOME/seedchamp/config.toml` (see `seedchamp config init`).

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use seedchamp_engine::{
    add_torrent, bench_catalog_fill_and_list, bench_list_existing, load_config, poll_watch_once,
    print_report, recheck_torrent, resolve_ltep_client, resolve_peer_id_prefix, serve_main,
    spawn_watcher, to_toml_string, tracker_user_agent, write_config_template, AddOptions, Catalog,
    Config, RuntimeConfig, UploadBackend, UploadOptions, WatchCallback,
};
use seedchamp_import::{import_session_with, ImportOptions};

#[derive(Parser, Debug)]
#[command(
    name = "seedchamp",
    version = seedchamp_engine::VERSION,
    about = "BitTorrent seedbox — TUI by default, headless serve, import tools"
)]
struct Cli {
    /// Config file (default: $XDG_CONFIG_HOME/seedchamp/config.toml)
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    /// SQLite catalog path (overrides config)
    #[arg(long, global = true)]
    db: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Interactive / list UI (default when no subcommand).
    Tui,
    /// Catalog / single-torrent operations.
    Torrent {
        /// JSON on stdout for scripts (array for list, object for other actions).
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        action: TorrentCmd,
    },
    /// Import torrents into the catalog from external sources.
    Import {
        #[command(subcommand)]
        action: ImportCmd,
    },
    /// Check catalog, config, and paths.
    Doctor,
    /// Headless swarm (same stack as TUI). Uses config + catalog want_start.
    Serve,
    /// Microbenchmarks and harness swarm overrides.
    Bench {
        #[command(subcommand)]
        action: BenchCmd,
    },
    /// Configuration file helpers.
    Config {
        #[command(subcommand)]
        action: ConfigCmd,
    },
    /// Run directory watchers only (no TUI). Uses `[watch]` from config.
    Watch {
        /// Scan once and exit (no loop).
        #[arg(long)]
        once: bool,
    },
    /// Print version (package + short git revision) and exit.
    Version,
}

#[derive(Subcommand, Debug)]
enum ImportCmd {
    /// Import an rtorrent session directory into the catalog.
    Rtorrent {
        /// Path to rtorrent session directory.
        session_dir: PathBuf,
        /// Parse only; do not write.
        #[arg(long)]
        dry_run: bool,
        /// Mark imported torrents want_start / started.
        #[arg(long)]
        start_after: bool,
        /// Fallback data root when .rtorrent has no directory (default: config paths.data_root).
        #[arg(long)]
        data_root: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum TorrentCmd {
    /// List torrents (human table; use --json for scripts).
    #[command(visible_alias = "ls")]
    List,
    /// Add a .torrent from a file path or HTTP(S) URL.
    Add {
        /// Path to .torrent or http(s):// URL.
        source: String,
        /// Directory for downloaded data files (default: config paths.data_root).
        #[arg(long)]
        data_root: Option<PathBuf>,
        /// Mark torrent want_start after add.
        #[arg(long)]
        start: bool,
        /// Save a copy of the .torrent under this directory (default: config paths.torrent_dir).
        #[arg(long)]
        save_torrent_dir: Option<PathBuf>,
        /// Do not write a local copy of the .torrent.
        #[arg(long)]
        no_save_torrent: bool,
    },
    /// Recheck torrent data on disk against piece hashes.
    Recheck {
        /// Torrent id or hex infohash (prefix ok if unique).
        torrent: String,
    },
    /// Set want_start (catalog only; serve/TUI pick up on start).
    Start {
        /// Torrent id or hex infohash (prefix ok if unique).
        torrent: String,
    },
    /// Clear want_start (catalog only).
    Stop {
        /// Torrent id or hex infohash (prefix ok if unique).
        torrent: String,
    },
    /// Soft-delete from catalog (must be stopped; payload files kept).
    #[command(visible_alias = "rm")]
    Del {
        /// Torrent id or hex infohash (prefix ok if unique).
        torrent: String,
    },
}

#[derive(Subcommand, Debug)]
enum BenchCmd {
    /// Catalog insert + list microbench.
    Catalog {
        /// Synthetic torrents to insert.
        #[arg(long, default_value = "200")]
        count: u32,
        /// List iterations.
        #[arg(long, default_value = "50")]
        iterations: u32,
        /// DB path (default: <data>/bench-catalog.sqlite).
        #[arg(long)]
        bench_db: Option<PathBuf>,
    },
    /// List existing catalog microbench.
    List {
        /// List iterations.
        #[arg(long, default_value = "50")]
        iterations: u32,
        /// DB path (default: --db / config catalog).
        #[arg(long)]
        bench_db: Option<PathBuf>,
    },
    /// Headless SessionRuntime with harness knobs (not for daily use).
    Swarm {
        /// Listen address (overrides config).
        #[arg(long)]
        listen: Option<String>,
        /// Wire encryption (overrides config): off | prefer-plain | prefer-rc4 | require-rc4.
        #[arg(long)]
        encryption: Option<String>,
        /// Force-start torrent id(s) (repeatable).
        #[arg(long = "torrent")]
        torrents: Vec<String>,
        /// Manual peer host:port (repeatable; dialed for every active torrent).
        #[arg(long = "peer")]
        peers: Vec<String>,
        /// Disable HTTP tracker announce.
        #[arg(long)]
        no_announce: bool,
        /// Seed upload I/O: auto | pread | compio (overrides config/env).
        #[arg(long = "upload-backend")]
        upload_backend: Option<String>,
        /// Request pipeline depth (blocks); default from config.
        #[arg(long)]
        pipeline: Option<usize>,
        /// After piece SHA-1, skip durable pwrite (wire-only). Upload disabled.
        #[arg(long)]
        discard_writes: bool,
    },
}

#[derive(Subcommand, Debug)]
enum ConfigCmd {
    /// Write a commented template to the config path.
    Init {
        /// Overwrite existing file.
        #[arg(long)]
        force: bool,
    },
    /// Print the effective merged config (CLI/env/file/defaults).
    Show,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    let result = run(cli);
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("seedchamp: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> seedchamp_engine::Result<()> {
    // No catalog/config required.
    if matches!(cli.command, Some(Commands::Version)) {
        println!("seedchamp {}", seedchamp_engine::VERSION);
        return Ok(());
    }

    // Config commands may run before a full load (init creates file).
    if let Some(Commands::Config { action }) = &cli.command {
        return match action {
            ConfigCmd::Init { force } => config_init(cli.config.as_deref(), *force),
            ConfigCmd::Show => {
                let (cfg, path) = load_and_merge(&cli)?;
                config_show(&cfg, &path)
            }
        };
    }

    let (cfg, cfg_path) = load_and_merge(&cli)?;
    let db = cfg.paths.db.clone();

    // Ensure data dirs exist for normal ops (best-effort; open will fail clearly if needed).
    let _ = ensure_parent_dir(&db);
    let _ = std::fs::create_dir_all(&cfg.paths.data_root);
    let _ = std::fs::create_dir_all(&cfg.paths.torrent_dir);

    // Config-primary limits + soft-delete purge + complete storage audit.
    if db.is_file()
        || matches!(
            cli.command,
            None | Some(Commands::Tui)
                | Some(Commands::Doctor)
                | Some(Commands::Serve)
                | Some(Commands::Bench {
                    action: BenchCmd::Swarm { .. },
                })
        )
    {
        if let Ok(mut cat) = Catalog::open(&db) {
            match cfg.apply_startup_to_catalog(&mut cat) {
                Ok(rep) => {
                    if rep.purged > 0 {
                        eprintln!(
                            "purged {} soft-deleted torrent(s) from catalog (older than {} day(s); files kept)",
                            rep.purged,
                            cfg.catalog.soft_delete_purge_days
                        );
                    }
                    if rep.storage.demoted > 0 {
                        eprintln!(
                            "storage audit: demoted {} complete torrent(s) (missing/wrong-size files; stopped; recheck after fix)",
                            rep.storage.demoted
                        );
                    }
                }
                Err(e) => {
                    eprintln!("catalog startup maintenance failed: {e}");
                }
            }
        }
    }

    match cli.command.unwrap_or(Commands::Tui) {
        Commands::Tui => {
            let rt = seedchamp_engine::RuntimeConfig::from_config(&cfg)?;
            let sort = seedchamp_tui::ListSort::from_tui_config(&cfg.tui);
            let config_dir = cfg_path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or_else(|| std::path::Path::new("."));
            // Best-effort: ensure stock theme files exist for user editing.
            let _ = seedchamp_tui::Theme::write_stock_themes(config_dir, false);
            let theme = seedchamp_tui::Theme::load(&cfg.tui.theme, config_dir)?;
            seedchamp_tui::run_with_settings_full_sort(
                &db,
                rt,
                cfg.limits.to_session_limits(),
                cfg.watch.clone(),
                cfg.paths.data_root.clone(),
                cfg.paths.torrent_dir.clone(),
                cfg.paths.leech_cache.clone(),
                cfg.paths.leech_cache_size,
                sort,
                theme,
            )
        }
        Commands::Watch { once } => watch_cmd(&db, &cfg, once),
        Commands::Torrent { json, action } => torrent_cmd(&db, &cfg, json, action),
        Commands::Import { action } => match action {
            ImportCmd::Rtorrent {
                session_dir,
                dry_run,
                start_after,
                data_root,
            } => {
                let data_root =
                    data_root.unwrap_or_else(|| cfg.paths.data_root.display().to_string());
                let opts = ImportOptions {
                    dry_run,
                    start_after,
                    default_data_root: data_root,
                };
                match import_session_with(&session_dir, &db, opts) {
                    Ok(report) => {
                        println!(
                            "import rtorrent: scanned={} imported={} skipped={} updated={} errors={}",
                            report.scanned,
                            report.imported,
                            report.skipped,
                            report.updated,
                            report.errors.len()
                        );
                        println!(
                            "  transfer totals from session: up={} down={} ({} torrent(s) with non-zero)",
                            report.uploaded_bytes,
                            report.downloaded_bytes,
                            report.with_transfer_stats
                        );
                        if report.with_transfer_stats == 0 && report.scanned > 0 {
                            eprintln!(
                                "  note: no total_uploaded/total_downloaded found in .rtorrent sidecars\n\
                                 \t(libtorrent_resume usually has no up/down keys; check session path)"
                            );
                        }
                        for e in report.errors {
                            eprintln!("  error: {e}");
                        }
                        Ok(())
                    }
                    Err(e) => Err(e),
                }
            }
        },
        Commands::Doctor => doctor(&db, &cfg, &cfg_path),
        Commands::Serve => serve_cmd(&db, &cfg),
        Commands::Bench { action } => match action {
            BenchCmd::Catalog {
                count,
                iterations,
                bench_db,
            } => {
                let default_bench = cfg
                    .paths
                    .db
                    .parent()
                    .unwrap_or_else(|| std::path::Path::new("."))
                    .join("bench-catalog.sqlite");
                bench_catalog_cmd(count, iterations, bench_db.or(Some(default_bench)))
            }
            BenchCmd::List {
                iterations,
                bench_db,
            } => bench_list_cmd(&db, iterations, bench_db),
            BenchCmd::Swarm {
                listen,
                encryption,
                torrents,
                peers,
                no_announce,
                upload_backend,
                pipeline,
                discard_writes,
            } => bench_swarm_cmd(
                &db,
                &cfg,
                listen.as_deref(),
                encryption.as_deref(),
                &torrents,
                &peers,
                no_announce,
                upload_backend.as_deref(),
                pipeline,
                discard_writes,
            ),
        },
        Commands::Config { .. } | Commands::Version => unreachable!("handled above"),
    }
}

/// Load file + env, then apply CLI overrides from global flags.
fn load_and_merge(cli: &Cli) -> seedchamp_engine::Result<(Config, PathBuf)> {
    let (mut cfg, path) = load_config(cli.config.as_deref())?;
    if let Some(ref db) = cli.db {
        cfg.paths.db = db.clone();
    }
    cfg.expand_paths();
    Ok((cfg, path))
}

fn config_init(explicit: Option<&std::path::Path>, force: bool) -> seedchamp_engine::Result<()> {
    let path = seedchamp_engine::resolve_config_path(explicit);
    write_config_template(&path, force)?;
    println!("wrote {}", path.display());
    if let Some(dir) = path.parent() {
        match seedchamp_tui::Theme::write_stock_themes(dir, force) {
            Ok(written) => {
                for p in written {
                    println!("wrote {}", p.display());
                }
            }
            Err(e) => eprintln!("theme stock files: {e}"),
        }
    }
    println!("  edit paths/network/limits, then: seedchamp config show");
    println!("  tui.theme = \"default\" | \"soft\" | path under themes/");
    Ok(())
}

fn config_show(cfg: &Config, path: &std::path::Path) -> seedchamp_engine::Result<()> {
    println!("# effective config (file: {})", path.display());
    println!("# exists: {}", path.is_file());
    print!("{}", to_toml_string(cfg)?);
    Ok(())
}

fn ensure_parent_dir(path: &std::path::Path) -> seedchamp_engine::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| seedchamp_engine::Error::Path(parent.to_path_buf(), e.to_string()))?;
        }
    }
    Ok(())
}

fn serve_cmd(db: &std::path::Path, cfg: &Config) -> seedchamp_engine::Result<()> {
    let rt = RuntimeConfig::from_config(cfg)?;
    let stop = Arc::new(AtomicBool::new(false));
    ctrlc_setup(stop.clone());
    println!("seedchamp serve (Ctrl+C to stop)");
    serve_main(db, rt, Vec::new(), false, stop)
}

fn bench_swarm_cmd(
    db: &std::path::Path,
    cfg: &Config,
    listen: Option<&str>,
    encryption: Option<&str>,
    torrents: &[String],
    peers: &[String],
    no_announce: bool,
    upload_backend: Option<&str>,
    pipeline: Option<usize>,
    discard_writes: bool,
) -> seedchamp_engine::Result<()> {
    let mut rt = RuntimeConfig::from_config(cfg)?;
    if let Some(enc) = encryption {
        rt.encryption = enc.parse().map_err(|e: seedchamp_engine::Error| {
            seedchamp_engine::Error::Msg(format!("bad --encryption {enc:?}: {e}"))
        })?;
    }
    if let Some(l) = listen {
        rt.listen = l
            .parse()
            .map_err(|e| seedchamp_engine::Error::Msg(format!("bad --listen: {e}")))?;
    }
    if no_announce {
        rt.announce = false;
    }
    if let Some(b) = upload_backend {
        rt.upload = UploadOptions {
            backend: UploadBackend::parse(b)?.resolve()?,
        };
    }
    if let Some(p) = pipeline {
        rt.pipeline = seedchamp_engine::clamp_initial_pipeline(
            p.max(1),
            rt.pipeline_max.max(seedchamp_engine::MIN_PIPELINE),
        );
    }
    rt.discard_writes = discard_writes;
    let mut force_ids = Vec::new();
    if !torrents.is_empty() {
        let cat = Catalog::open(db)?;
        for t in torrents {
            force_ids.push(cat.resolve_torrent_ref(t)?);
        }
    }
    let mut peer_addrs = Vec::new();
    for p in peers {
        peer_addrs.push(
            p.parse()
                .map_err(|e| seedchamp_engine::Error::Msg(format!("bad --peer {p}: {e}")))?,
        );
    }
    rt.manual_peers = peer_addrs;

    let stop = Arc::new(AtomicBool::new(false));
    ctrlc_setup(stop.clone());
    println!("seedchamp bench swarm (Ctrl+C to stop)");
    // exit_when_complete: harness leech waits for process exit after download.
    serve_main(db, rt, force_ids, true, stop)
}

fn ctrlc_setup(stop: Arc<AtomicBool>) {
    let _ = ctrlc::set_handler(move || {
        stop.store(true, Ordering::SeqCst);
    });
}

fn torrent_cmd(
    db: &std::path::Path,
    cfg: &Config,
    json: bool,
    action: TorrentCmd,
) -> seedchamp_engine::Result<()> {
    match action {
        TorrentCmd::List => torrent_list_cmd(db, json),
        TorrentCmd::Add {
            source,
            data_root,
            start,
            save_torrent_dir,
            no_save_torrent,
        } => {
            let data_root = data_root.unwrap_or_else(|| cfg.paths.data_root.clone());
            torrent_add_cmd(
                db,
                &source,
                data_root,
                cfg.paths.leech_cache.clone(),
                cfg.paths.leech_cache_size,
                start,
                save_torrent_dir.or_else(|| Some(cfg.paths.torrent_dir.clone())),
                no_save_torrent,
                json,
            )
        }
        TorrentCmd::Recheck { torrent } => torrent_recheck_cmd(db, &torrent, json),
        TorrentCmd::Start { torrent } => torrent_want_start_cmd(db, &torrent, true, json),
        TorrentCmd::Stop { torrent } => torrent_want_start_cmd(db, &torrent, false, json),
        TorrentCmd::Del { torrent } => torrent_del_cmd(db, &torrent, json),
    }
}

fn torrent_list_cmd(db: &std::path::Path, json: bool) -> seedchamp_engine::Result<()> {
    if json {
        if !db.exists() {
            return Err(seedchamp_engine::Error::Msg(format!(
                "catalog missing: {}",
                db.display()
            )));
        }
        let cat = Catalog::open(db)?;
        let rows = cat.list_torrents()?;
        let arr: Vec<serde_json::Value> = rows
            .iter()
            .map(|r| {
                serde_json::json!({
                    "id": r.id,
                    "name": r.name,
                    "want_start": r.want_start,
                    "complete": r.complete,
                    "have": r.have_count,
                    "pieces": r.piece_count,
                    "size": r.total_size,
                    "state": r.state,
                    "infohash": r.infohash_hex,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string(&arr).unwrap_or_else(|_| "[]".into())
        );
        return Ok(());
    }
    // Human table (scripts should use --json).
    seedchamp_tui::run_plain_list(db)
}

fn torrent_add_cmd(
    db: &std::path::Path,
    source: &str,
    data_root: PathBuf,
    leech_cache: PathBuf,
    leech_cache_size: u64,
    start: bool,
    save_torrent_dir: Option<PathBuf>,
    no_save_torrent: bool,
    json: bool,
) -> seedchamp_engine::Result<()> {
    let save = if no_save_torrent {
        None
    } else {
        save_torrent_dir
    };
    let opts = AddOptions {
        data_root,
        leech_cache,
        leech_cache_size,
        start,
        save_torrent_dir: save,
    };
    let mut cat = Catalog::open(db)?;
    let report = add_torrent(&mut cat, source, &opts)?;
    if json {
        println!(
            "{}",
            serde_json::json!({
                "id": report.id,
                "action": "add",
                "name": report.name,
                "infohash": report.infohash_hex,
                "size": report.total_size,
                "pieces": report.piece_count,
                "existed": report.already_existed,
                "want_start": start,
            })
        );
        return Ok(());
    }
    let status = if report.already_existed {
        "exists"
    } else {
        "added"
    };
    println!(
        "add: {status} id={} name={:?} infohash={} size={} pieces={} trackers={} want_start={}",
        report.id,
        report.name,
        report.infohash_hex,
        report.total_size,
        report.piece_count,
        report.trackers,
        start,
    );
    println!("  source: {}", report.source);
    if let Some(p) = &report.saved_torrent {
        println!("  saved:  {}", p.display());
    }
    if start && !report.already_existed {
        println!("  next:   seedchamp serve   # or open the TUI");
    } else if !report.already_existed {
        println!(
            "  next:   seedchamp torrent start {}   # or TUI Ctrl+s",
            report.id
        );
    }
    Ok(())
}

fn torrent_recheck_cmd(
    db: &std::path::Path,
    spec: &str,
    json: bool,
) -> seedchamp_engine::Result<()> {
    let mut cat = Catalog::open(db)?;
    let id = cat.resolve_torrent_ref(spec)?;
    let report = recheck_torrent(&mut cat, id)?;
    if json {
        println!(
            "{}",
            serde_json::json!({
                "id": id,
                "action": "recheck",
                "complete": report.complete,
                "good": report.good,
                "bad": report.bad,
                "missing": report.missing,
                "pieces": report.piece_count,
            })
        );
        return Ok(());
    }
    println!("recheck: torrent id={id} …");
    println!(
        "recheck done: pieces={} good={} bad={} missing={} complete={}",
        report.piece_count, report.good, report.bad, report.missing, report.complete
    );
    Ok(())
}

fn torrent_want_start_cmd(
    db: &std::path::Path,
    spec: &str,
    want: bool,
    json: bool,
) -> seedchamp_engine::Result<()> {
    let mut cat = Catalog::open(db)?;
    let id = cat.resolve_torrent_ref(spec)?;
    cat.set_want_start(id, want)?;
    let action = if want { "start" } else { "stop" };
    if json {
        println!(
            "{}",
            serde_json::json!({
                "id": id,
                "action": action,
                "want_start": want,
            })
        );
        return Ok(());
    }
    if want {
        println!("torrent #{id} started (want_start; run serve or TUI to swarm)");
    } else {
        println!("torrent #{id} stopped (want_start cleared)");
    }
    Ok(())
}

fn torrent_del_cmd(db: &std::path::Path, spec: &str, json: bool) -> seedchamp_engine::Result<()> {
    let mut cat = Catalog::open(db)?;
    let id = cat.resolve_torrent_ref(spec)?;
    cat.mark_deleted(id)?;
    if json {
        println!(
            "{}",
            serde_json::json!({
                "id": id,
                "action": "del",
                "deleted": true,
            })
        );
        return Ok(());
    }
    println!("torrent #{id} soft-deleted (payload kept; catalog purge after configured days)");
    Ok(())
}

fn watch_cmd(db: &std::path::Path, cfg: &Config, once: bool) -> seedchamp_engine::Result<()> {
    if !cfg.watch.enabled {
        return Err(seedchamp_engine::Error::Msg(
            "watch disabled (set watch.enabled = true in config)".into(),
        ));
    }
    if cfg.active_watch_dirs().is_empty() {
        return Err(seedchamp_engine::Error::Msg(
            "no active watch.dirs (add [[watch.dirs]] entries)".into(),
        ));
    }
    let on_load: WatchCallback = Arc::new(|ev| {
        println!(
            "watch[{}]: {} id={} {} data_root_stamped start={} deleted={}",
            ev.watch_name,
            if ev.report.already_existed {
                "exists"
            } else {
                "added"
            },
            ev.report.id,
            ev.report.name,
            ev.start,
            ev.deleted_after_import
        );
        // Headless watch only sets want_start via AddOptions; user runs tui or serve.
    });
    if once {
        let events = poll_watch_once(
            db,
            &cfg.watch,
            &cfg.paths.data_root,
            &cfg.paths.torrent_dir,
            &cfg.paths.leech_cache,
            cfg.paths.leech_cache_size,
            Some(&on_load),
        )?;
        println!("watch --once: {} event(s)", events.len());
        return Ok(());
    }
    println!(
        "seedchamp watch (Ctrl+C to stop) · {} dir(s) every {}s",
        cfg.active_watch_dirs().len(),
        cfg.watch.interval_secs.max(1)
    );
    let handle = spawn_watcher(
        db.to_path_buf(),
        cfg.watch.clone(),
        cfg.paths.data_root.clone(),
        cfg.paths.torrent_dir.clone(),
        cfg.paths.leech_cache.clone(),
        cfg.paths.leech_cache_size,
        Some(on_load),
    )?;
    let stop = Arc::new(AtomicBool::new(false));
    let stop_c = stop.clone();
    ctrlc_setup(stop_c);
    while !stop.load(Ordering::SeqCst) {
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    handle.stop();
    Ok(())
}

fn doctor(
    db: &std::path::Path,
    cfg: &Config,
    cfg_path: &std::path::Path,
) -> seedchamp_engine::Result<()> {
    println!(
        "seedchamp doctor\n  engine={}\n  config={}\n  config_exists={}\n  db={}\n  encryption={} (provide={:#x})",
        seedchamp_engine::VERSION,
        cfg_path.display(),
        cfg_path.is_file(),
        db.display(),
        cfg.network.encryption,
        cfg.encryption_mode()?.crypto_provide_bits(),
    );
    // Effective identity (same resolution as RuntimeConfig::from_config).
    let peer_prefix = resolve_peer_id_prefix(&cfg.network.peer_id_prefix);
    let peer_prefix_disp = String::from_utf8_lossy(&peer_prefix);
    let ua = {
        let t = cfg.network.http_user_agent.trim();
        if t.is_empty() {
            tracker_user_agent()
        } else {
            t
        }
    };
    let ltep = {
        let t = cfg.network.ltep_client.trim();
        if t.is_empty() {
            resolve_ltep_client(&cfg.network.peer_id_prefix)
        } else {
            t.to_string()
        }
    };
    println!(
        "  identity: peer_id_prefix={} → {}  user_agent={}  ltep_v={}",
        cfg.network.peer_id_prefix, peer_prefix_disp, ua, ltep
    );
    println!(
        "  listen={} announce={} upload.backend={} pipeline={} peer_workers={} hash_workers={}",
        cfg.network.listen,
        cfg.network.announce,
        cfg.upload.backend,
        cfg.swarm.pipeline,
        if cfg.swarm.peer_workers == 0 {
            "auto".into()
        } else {
            cfg.swarm.peer_workers.to_string()
        },
        if cfg.swarm.hash_workers == 0 {
            "auto".into()
        } else {
            cfg.swarm.hash_workers.to_string()
        }
    );
    println!(
        "  sockbuf send={} recv={} (0=OS default)",
        cfg.network.send_buffer_bytes, cfg.network.recv_buffer_bytes
    );
    println!(
        "  tracker: max_per_host={} stagger_ms={} max_inflight={}",
        cfg.tracker.max_concurrent_per_host,
        cfg.tracker.startup_stagger_ms,
        cfg.tracker.max_inflight_announces
    );
    println!(
        "  data_root={} torrent_dir={}",
        cfg.paths.data_root.display(),
        cfg.paths.torrent_dir.display()
    );
    if cfg.paths.leech_cache.as_os_str().is_empty() {
        println!("  leech_cache=(disabled)");
    } else {
        let free = seedchamp_engine::free_space_bytes(&cfg.paths.leech_cache)
            .map(|n| format!("{n} free bytes"))
            .unwrap_or_else(|e| format!("probe failed: {e}"));
        let reserved = Catalog::open(db)
            .and_then(|c| c.leech_cache_reserved_bytes())
            .map(|n| format!("{n} reserved"))
            .unwrap_or_else(|e| format!("reserved probe failed: {e}"));
        let cap = if cfg.paths.leech_cache_size == 0 {
            "size_cap=off".into()
        } else {
            format!("size_cap={}", cfg.paths.leech_cache_size)
        };
        println!(
            "  leech_cache={} ({}; {}; {})",
            cfg.paths.leech_cache.display(),
            free,
            reserved,
            cap
        );
    }
    println!(
        "  limits (config primary): upload_bps={} download_bps={} min_peers={} max_peers={} seed_dial_peers={} max_connections={}",
        cfg.limits.max_upload_bps,
        cfg.limits.max_download_bps,
        cfg.limits.clamped_peer_limits().0,
        cfg.limits.clamped_peer_limits().1,
        cfg.limits.seed_dial_peers,
        cfg.limits.max_connections
    );
    let active = cfg.active_watch_dirs();
    println!(
        "  watch: enabled={} interval={}s dirs={}/{}",
        cfg.watch.enabled,
        cfg.watch.interval_secs,
        active.len(),
        cfg.watch.dirs.len()
    );
    for d in active {
        let tmpl = d
            .dl_path
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or("(paths.data_root)");
        let label = d.name.clone().unwrap_or_else(|| {
            d.path
                .file_name()
                .map(|s| s.to_string_lossy().into())
                .unwrap_or_default()
        });
        let ctx = seedchamp_engine::DlPathContext::watch_only(&label);
        let resolved = seedchamp_engine::resolve_dl_path(d, &cfg.paths.data_root, &ctx);
        println!(
            "    - {} path={} dl_path={} → {} start={} delete_after_import={}",
            label,
            d.path.display(),
            tmpl,
            resolved.display(),
            d.start,
            d.delete_after_import
        );
        if !d.path.is_dir() {
            println!("      (missing directory)");
        }
    }
    if !db.is_file() {
        println!("  catalog: missing (run add/import or config init + add)");
        return Ok(());
    }
    let cat = Catalog::open(db)?;
    let rows = cat.list_torrents()?;
    let complete = rows.iter().filter(|r| r.complete).count();
    let want = rows.iter().filter(|r| r.want_start).count();
    let lim = cat.session_limits()?;
    println!(
        "  torrents: {} (complete={complete} want_start={want})",
        rows.len()
    );
    println!(
        "  catalog limits mirror: upload_bps={} download_bps={} min_peers={} max_peers={}",
        lim.max_upload_bps, lim.max_download_bps, lim.min_peers, lim.max_peers
    );
    if let Some(rss) = seedchamp_engine::current_rss_bytes() {
        println!("  rss_bytes={rss}");
    }
    Ok(())
}

fn bench_catalog_cmd(
    count: u32,
    iterations: u32,
    bench_db: Option<PathBuf>,
) -> seedchamp_engine::Result<()> {
    let path = bench_db.unwrap_or_else(|| PathBuf::from("data/bench-catalog.sqlite"));
    let reports = bench_catalog_fill_and_list(&path, count, iterations)?;
    print_report(&mut std::io::stdout(), &reports)?;
    Ok(())
}

fn bench_list_cmd(
    db: &std::path::Path,
    iterations: u32,
    bench_db: Option<PathBuf>,
) -> seedchamp_engine::Result<()> {
    let path = bench_db.unwrap_or_else(|| db.to_path_buf());
    if !path.is_file() {
        return Err(seedchamp_engine::Error::Msg(format!(
            "bench list: db missing {}",
            path.display()
        )));
    }
    let report = bench_list_existing(&path, iterations)?;
    print_report(&mut std::io::stdout(), &[report])?;
    Ok(())
}
