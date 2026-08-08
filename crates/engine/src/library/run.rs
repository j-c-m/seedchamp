//! Headless entry into the shared [`SessionRuntime`] (`serve` / `bench swarm`).
//!
//! TUI and headless CLI share one engine. Incomplete active torrents always
//! upload what they have (unless [`RuntimeConfig::discard_writes`]).

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::catalog::Catalog;
use crate::error::{Error, Result};
use crate::session::{RuntimeConfig, SessionRuntime};

/// Blocking headless swarm until Ctrl+C (or targets complete when configured).
///
/// Same stack as TUI. Activates catalog `want_start` via session bootstrap;
/// force-starts `force_start` ids when non-empty (bench).
///
/// When `exit_when_complete` is true and any of those targets (or want_start
/// incompletes if force list empty) are incomplete, exit after they finish
/// (harness leecher). Pure seeders wait until `stop`.
pub fn serve_main(
    db_path: &Path,
    rt: RuntimeConfig,
    force_start: Vec<i64>,
    exit_when_complete: bool,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    if rt.discard_writes {
        tracing::warn!("discard_writes: no durable pwrite; upload disabled (no payload to serve)");
        println!("serve: discard_writes=on (SHA-1 only; no piece pwrite)");
    }

    if !force_start.is_empty() {
        let cat = Catalog::open(db_path)?;
        let rows = cat.list_torrents()?;
        for &id in &force_start {
            if !rows.iter().any(|r| r.id == id) {
                return Err(Error::Msg(format!("torrent id={id} not in catalog")));
            }
        }
    } else {
        let cat = Catalog::open(db_path)?;
        let want = cat.list_want_start_ids()?;
        if want.is_empty() {
            return Err(Error::Msg(
                "no torrents with want_start (start one in the TUI, use add --start, or bench swarm --torrent)"
                    .into(),
            ));
        }
    }

    let wait_ids = incomplete_wait_ids(db_path, &force_start, exit_when_complete)?;
    let listen = rt.listen;
    let encryption = rt.encryption;
    let manual_peers = rt.manual_peers.len();

    let session = SessionRuntime::start(db_path, rt)?;

    for &id in &force_start {
        session.start_torrent(id)?;
        if let Some((name, have, pc, left)) = torrent_brief(db_path, id) {
            println!("serve: activated id={id} name={name} have={have}/{pc} left={left}");
        } else {
            println!("serve: activated id={id}");
        }
    }

    // Brief settle so want_start bootstrap can load.
    thread::sleep(Duration::from_millis(100));
    let active = session.snapshot().torrents.len();
    println!(
        "serve: listening on {} encryption={} io_workers={} hash_workers={} active≈{} manual_peers={}",
        listen,
        encryption,
        session.peer_workers(),
        session.hash_workers(),
        active,
        manual_peers
    );

    if exit_when_complete && !wait_ids.is_empty() {
        let mut last_have: HashMap<i64, u32> = HashMap::new();
        while !stop.load(Ordering::SeqCst) {
            let snap = session.snapshot();
            let mut all_done = true;
            for &id in &wait_ids {
                let live = snap.torrents.iter().find(|t| t.id == id);
                match live {
                    Some(t) if t.complete => {
                        if last_have.get(&id).copied() != Some(t.have_count) {
                            println!("serve: torrent id={id} complete");
                            last_have.insert(id, t.have_count);
                        }
                    }
                    Some(t) => {
                        all_done = false;
                        let prev = last_have.get(&id).copied().unwrap_or(0);
                        if t.have_count != prev
                            && (t.have_count % 10 == 0
                                || t.have_count < 20
                                || t.have_count > prev + 5)
                        {
                            println!(
                                "serve: have {}/{} pieces (id={id})",
                                t.have_count, t.piece_count
                            );
                            last_have.insert(id, t.have_count);
                        }
                    }
                    None => {
                        all_done = false;
                    }
                }
            }
            if all_done {
                // Wanted-complete can race ahead of paths.leech_cache handoff
                // (copy → permanent root). Hold exit until home_root is cleared.
                let handoff_pending = match Catalog::open(db_path) {
                    Ok(cat) => wait_ids
                        .iter()
                        .any(|&id| matches!(cat.get_home_root(id), Ok(Some(_)))),
                    Err(_) => false,
                };
                if handoff_pending {
                    thread::sleep(Duration::from_millis(100));
                    continue;
                }
                println!("serve: all target torrents complete");
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        session.shutdown();
    } else {
        while !stop.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(200));
        }
        session.shutdown();
    }
    Ok(())
}

fn incomplete_wait_ids(
    db_path: &Path,
    force_start: &[i64],
    exit_when_complete: bool,
) -> Result<Vec<i64>> {
    if !exit_when_complete {
        return Ok(Vec::new());
    }
    let cat = Catalog::open(db_path)?;
    let rows = cat.list_torrents()?;
    let candidates: Vec<i64> = if force_start.is_empty() {
        cat.list_want_start_ids()?
    } else {
        force_start.to_vec()
    };
    Ok(candidates
        .into_iter()
        .filter(|&id| {
            rows.iter()
                .find(|r| r.id == id)
                .map(|r| !r.complete)
                .unwrap_or(false)
        })
        .collect())
}

fn torrent_brief(db_path: &Path, id: i64) -> Option<(String, u32, u32, u64)> {
    let cat = Catalog::open(db_path).ok()?;
    let row = cat.list_torrents().ok()?.into_iter().find(|r| r.id == id)?;
    let left = if row.complete {
        0
    } else if let Ok(lay) = cat.load_storage_layout(id) {
        let full = (row.have_count as u64).saturating_mul(lay.piece_length as u64);
        lay.total_size.saturating_sub(full.min(lay.total_size))
    } else {
        row.total_size
    };
    Some((row.name, row.have_count, row.piece_count, left))
}
