//! Control thread and process spawn.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use parking_lot::RwLock;

use crate::catalog::Catalog;
use crate::error::{Error, Result};
use crate::runtime::default_peer_workers;
use crate::session::{RuntimeConfig, SessionRuntime};

use super::handle::{ControlHandle, ControlPlane};
use super::mutation::mutation_worker;
use super::reader::catalog_reader_worker;
use super::types::{CatalogReadJob, ControlEvent, EngineCommand, MutationJob};

/// Start control plane + peer worker pool. Returns handle for the UI thread.
pub fn spawn_control_plane(
    db: &Path,
    mut cfg: RuntimeConfig,
) -> Result<(ControlHandle, ControlPlane)> {
    if cfg.peer_workers.is_none() {
        cfg.peer_workers = Some(default_peer_workers());
    }
    let db = db.to_path_buf();
    let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCommand>();
    let (event_tx, event_rx) = mpsc::channel::<ControlEvent>();
    let session_slot: Arc<RwLock<Option<SessionRuntime>>> = Arc::new(RwLock::new(None));

    let handle = ControlHandle {
        cmd_tx,
        event_rx: Arc::new(std::sync::Mutex::new(event_rx)),
        session: session_slot.clone(),
    };

    let join = thread::Builder::new()
        .name("seedchamp-control".into())
        .spawn(move || {
            if let Err(e) = control_thread_main(db, cfg, cmd_rx, event_tx, session_slot) {
                tracing::error!(error = %e, "control plane exited");
            }
        })
        .map_err(|e| Error::Msg(format!("spawn control plane: {e}")))?;

    Ok((handle, ControlPlane { join: Some(join) }))
}

fn control_thread_main(
    db: PathBuf,
    cfg: RuntimeConfig,
    cmd_rx: mpsc::Receiver<EngineCommand>,
    event_tx: mpsc::Sender<ControlEvent>,
    session_slot: Arc<RwLock<Option<SessionRuntime>>>,
) -> Result<()> {
    let _ = Catalog::open(&db)?;

    let session = SessionRuntime::start(&db, cfg)?;
    let workers = session.peer_workers();
    let listen = session.snapshot_nonblocking().listen;
    *session_slot.write() = Some(session.clone());
    let _ = event_tx.send(ControlEvent::Ready {
        listen: listen.clone(),
        peer_workers: workers,
    });
    let _ = event_tx.send(ControlEvent::Status(format!(
        "control ready · {workers} io workers · {listen}"
    )));

    // Serial mutation worker: start→stop→start never races; actually mutates catalog + hot set.
    let (mut_tx, mut_rx) = mpsc::channel::<MutationJob>();
    let session_m = session.clone();
    let event_m = event_tx.clone();
    let db_m = db.clone();
    let mut_tx_for_worker = mut_tx.clone();
    let mut_join = thread::Builder::new()
        .name("seedchamp-mutate".into())
        .spawn(move || mutation_worker(session_m, db_m, mut_rx, mut_tx_for_worker, event_m))
        .map_err(|e| Error::Msg(format!("spawn mutate: {e}")))?;

    // Catalog RO worker: TUI full list (and future read-only SQL). Short busy timeout.
    let (read_tx, read_rx) = mpsc::channel::<CatalogReadJob>();
    let event_r = event_tx.clone();
    let db_r = db.clone();
    let read_join = thread::Builder::new()
        .name("seedchamp-cread".into())
        .spawn(move || catalog_reader_worker(db_r, read_rx, event_r))
        .map_err(|e| Error::Msg(format!("spawn catalog reader: {e}")))?;

    loop {
        // 250ms is fine for UI latency; 50ms was a steady wake on FreeBSD at idle.
        match cmd_rx.recv_timeout(Duration::from_millis(250)) {
            Ok(EngineCommand::StartTorrent { id }) => {
                let _ = event_tx.send(ControlEvent::Status(format!("#{id} starting…")));
                if mut_tx.send(MutationJob::Start(id)).is_err() {
                    let _ = event_tx.send(ControlEvent::StartFailed {
                        id,
                        error: "mutation worker dead".into(),
                    });
                    break;
                }
            }
            Ok(EngineCommand::StopTorrent { id }) => {
                let _ = event_tx.send(ControlEvent::Status(format!("#{id} stop queued")));
                if mut_tx.send(MutationJob::Stop(id)).is_err() {
                    let _ = event_tx.send(ControlEvent::StopFailed {
                        id,
                        error: "mutation worker dead".into(),
                    });
                    break;
                }
            }
            Ok(EngineCommand::Recheck { id }) => {
                let _ = event_tx.send(ControlEvent::Status(format!("#{id} recheck queued")));
                if mut_tx.send(MutationJob::Recheck(id)).is_err() {
                    break;
                }
            }
            Ok(EngineCommand::SetFilePriority {
                torrent_id,
                file_idx,
                priority,
            }) => {
                if mut_tx
                    .send(MutationJob::SetFilePriority {
                        torrent_id,
                        file_idx,
                        priority,
                    })
                    .is_err()
                {
                    break;
                }
            }
            Ok(EngineCommand::Relocate { id, new_root }) => {
                let _ = event_tx.send(ControlEvent::Status(format!(
                    "#{id} relocating → {}…",
                    new_root.display()
                )));
                if mut_tx.send(MutationJob::Relocate { id, new_root }).is_err() {
                    let _ = event_tx.send(ControlEvent::RelocateFailed {
                        id,
                        error: "mutation worker dead".into(),
                    });
                    break;
                }
            }
            Ok(EngineCommand::SoftDelete { id }) => {
                let _ = event_tx.send(ControlEvent::Status(format!("#{id} deleting…")));
                if mut_tx.send(MutationJob::SoftDelete(id)).is_err() {
                    let _ = event_tx.send(ControlEvent::SoftDeleteFailed {
                        id,
                        error: "mutation worker dead".into(),
                    });
                    break;
                }
            }
            Ok(EngineCommand::Remove { id }) => {
                let _ = event_tx.send(ControlEvent::Status(format!("#{id} removing…")));
                if mut_tx.send(MutationJob::Remove(id)).is_err() {
                    let _ = event_tx.send(ControlEvent::RemoveFailed {
                        id,
                        error: "mutation worker dead".into(),
                    });
                    break;
                }
            }
            Ok(EngineCommand::SetSessionLimits { limits }) => {
                if mut_tx.send(MutationJob::SetSessionLimits(limits)).is_err() {
                    let _ = event_tx.send(ControlEvent::LimitsFailed {
                        error: "mutation worker dead".into(),
                    });
                    break;
                }
            }
            Ok(EngineCommand::ListCatalog { filter }) => {
                if read_tx
                    .send(CatalogReadJob::ListCatalog { filter })
                    .is_err()
                {
                    break;
                }
            }
            Ok(EngineCommand::Shutdown) => {
                let _ = read_tx.send(CatalogReadJob::Shutdown);
                let _ = mut_tx.send(MutationJob::Shutdown);
                break;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                let _ = read_tx.send(CatalogReadJob::Shutdown);
                let _ = mut_tx.send(MutationJob::Shutdown);
                break;
            }
        }
    }

    drop(read_tx);
    drop(mut_tx);
    let _ = read_join.join();
    let _ = mut_join.join();
    // Quit-time event=stopped + peer stop + Compio PeerWorkerPool::shutdown on this
    // (non-async) thread. Keep the session slot until finished so
    // ControlHandle::shutdown waits for tracker announces (not just detach).
    session.shutdown();
    *session_slot.write() = None;
    Ok(())
}
