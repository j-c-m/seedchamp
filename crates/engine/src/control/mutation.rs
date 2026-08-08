//! Serial mutation worker.

use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;

use crate::catalog::Catalog;
use crate::error::Result;
use crate::runtime::recheck_torrent_with_pool;
use crate::session::SessionRuntime;

use super::types::{ControlEvent, MutationJob};

pub(super) fn mutation_worker(
    session: SessionRuntime,
    db: PathBuf,
    rx: mpsc::Receiver<MutationJob>,
    // Detached recheck → ordered stop/start after bitfield commit.
    mut_tx: mpsc::Sender<MutationJob>,
    events: mpsc::Sender<ControlEvent>,
) {
    while let Ok(job) = rx.recv() {
        match job {
            MutationJob::Shutdown => break,
            MutationJob::Start(id) => match session.start_torrent(id) {
                Ok(()) => {
                    let name = torrent_name_hint(&session, id);
                    tracing::info!(id, torrent = %name, "start ok");
                    let _ = events.send(ControlEvent::Started { id });
                    let _ =
                        events.send(ControlEvent::Status(format!("#{id} started — announcing")));
                }
                Err(e) => {
                    tracing::error!(id, error = %e, "start failed");
                    let _ = events.send(ControlEvent::StartFailed {
                        id,
                        error: e.to_string(),
                    });
                }
            },
            MutationJob::Stop(id) => {
                let name = torrent_name_hint(&session, id);
                match session.stop_torrent(id) {
                    Ok(()) => {
                        tracing::info!(id, torrent = %name, "stop ok");
                        let _ = events.send(ControlEvent::Stopped { id });
                        let _ = events.send(ControlEvent::Status(format!("#{id} stopped")));
                    }
                    Err(e) => {
                        tracing::error!(id, torrent = %name, error = %e, "stop failed");
                        let _ = events.send(ControlEvent::StopFailed {
                            id,
                            error: e.to_string(),
                        });
                    }
                }
            }
            MutationJob::SyncAfterRecheck { id, start } => {
                // Catalog bitfield already written by the recheck thread.
                let name = torrent_name_hint(&session, id);
                if session.is_hot(id) {
                    if let Err(e) = session.stop_torrent(id) {
                        tracing::warn!(id, error = %e, "sync-after-recheck stop failed");
                    }
                }
                if start {
                    match session.start_torrent(id) {
                        Ok(()) => {
                            tracing::info!(id, torrent = %name, "sync-after-recheck start ok");
                            let _ = events.send(ControlEvent::Started { id });
                            let _ = events.send(ControlEvent::Status(format!(
                                "#{id} restarted after recheck"
                            )));
                        }
                        Err(e) => {
                            tracing::error!(id, error = %e, "sync-after-recheck start failed");
                            let _ = events.send(ControlEvent::StartFailed {
                                id,
                                error: e.to_string(),
                            });
                        }
                    }
                } else {
                    let _ = events.send(ControlEvent::Status(format!(
                        "#{id} recheck applied (stopped)"
                    )));
                }
            }
            MutationJob::SetFilePriority {
                torrent_id,
                file_idx,
                priority,
            } => {
                if let Err(e) = session.set_file_priority(torrent_id, file_idx, priority) {
                    tracing::warn!(
                        torrent_id,
                        file_idx,
                        error = %e,
                        "set_file_priority failed"
                    );
                    let _ = events.send(ControlEvent::Status(format!(
                        "#{torrent_id} file {file_idx} priority failed: {e}"
                    )));
                } else {
                    let on = priority > 0;
                    tracing::info!(id = torrent_id, file_idx, priority, on, "file priority");
                    let label = if on { "on" } else { "off" };
                    let _ = events.send(ControlEvent::Status(format!(
                        "#{torrent_id} file {file_idx} {label}"
                    )));
                }
            }
            MutationJob::Relocate { id, new_root } => {
                let name = torrent_name_hint(&session, id);
                match session.relocate_data_root(id, &new_root) {
                    Ok(rep) => {
                        tracing::info!(id, torrent = %name, note = %rep.note, "relocate ok");
                        let _ = events.send(ControlEvent::Relocated {
                            id,
                            data_root: rep.data_root,
                            note: rep.note,
                        });
                    }
                    Err(e) => {
                        tracing::error!(id, torrent = %name, error = %e, "relocate failed");
                        let _ = events.send(ControlEvent::RelocateFailed {
                            id,
                            error: e.to_string(),
                        });
                    }
                }
            }
            MutationJob::SoftDelete(id) => {
                let name = torrent_name_hint(&session, id);
                match session.soft_delete_torrent(id) {
                    Ok(()) => {
                        tracing::info!(id, torrent = %name, "soft-delete ok");
                        let _ = events.send(ControlEvent::SoftDeleted { id });
                        let _ = events.send(ControlEvent::Status(format!("#{id} deleted (soft)")));
                    }
                    Err(e) => {
                        tracing::warn!(id, torrent = %name, error = %e, "soft-delete failed");
                        let _ = events.send(ControlEvent::SoftDeleteFailed {
                            id,
                            error: e.to_string(),
                        });
                    }
                }
            }
            MutationJob::Remove(id) => {
                let name = torrent_name_hint(&session, id);
                match session.remove_torrent_catalog(id) {
                    Ok(()) => {
                        tracing::info!(id, torrent = %name, "catalog remove ok");
                        let _ = events.send(ControlEvent::Removed { id });
                        let _ = events.send(ControlEvent::Status(format!("#{id} removed")));
                    }
                    Err(e) => {
                        tracing::warn!(id, torrent = %name, error = %e, "catalog remove failed");
                        let _ = events.send(ControlEvent::RemoveFailed {
                            id,
                            error: e.to_string(),
                        });
                    }
                }
            }
            MutationJob::SetSessionLimits(limits) => match session.apply_session_limits(&limits) {
                Ok(()) => {
                    tracing::info!(
                        up = limits.max_upload_bps,
                        down = limits.max_download_bps,
                        max_peers = limits.max_peers,
                        "session limits applied"
                    );
                    let _ = events.send(ControlEvent::LimitsUpdated { limits });
                }
                Err(e) => {
                    tracing::error!(error = %e, "session limits failed");
                    let _ = events.send(ControlEvent::LimitsFailed {
                        error: e.to_string(),
                    });
                }
            },
            MutationJob::Recheck(id) => {
                // Detach immediately so start/stop are not stuck behind a long recheck.
                if !session.try_begin_recheck(id) {
                    let _ = events.send(ControlEvent::Status(format!(
                        "#{id} recheck already in progress"
                    )));
                    continue;
                }
                let name = torrent_name_hint(&session, id);
                let was_hot = session.is_hot(id);
                tracing::info!(
                    id,
                    torrent = %name,
                    hash_workers = session.hash_workers(),
                    was_hot,
                    "recheck begin (detached, hash pool)"
                );
                let _ = events.send(ControlEvent::Status(format!(
                    "#{id} recheck started (background)"
                )));

                let events_p = events.clone();
                let pool = session.hash_pool();
                let session_r = session.clone();
                let db_r = db.clone();
                let mut_tx_r = mut_tx.clone();
                let name_r = name.clone();
                if let Err(e) = thread::Builder::new()
                    .name(format!("seedchamp-recheck-{id}"))
                    .spawn(move || {
                        let events_prog = events_p.clone();
                        let r: Result<_> = (|| {
                            let mut cat = Catalog::open(&db_r)?;
                            recheck_torrent_with_pool(&mut cat, id, &pool, |p| {
                                let _ = events_prog.send(ControlEvent::RecheckProgress {
                                    id: p.torrent_id,
                                    piece_count: p.piece_count,
                                    checked: p.checked,
                                    good: p.good,
                                    bad: p.bad,
                                    missing: p.missing,
                                });
                            })
                        })();

                        session_r.end_recheck(id);

                        match r {
                            Ok(report) => {
                                let message = format!(
                                    "recheck id={id}: good={} bad={} missing={} complete={}",
                                    report.good, report.bad, report.missing, report.complete
                                );
                                tracing::info!(
                                    id,
                                    torrent = %name_r,
                                    good = report.good,
                                    bad = report.bad,
                                    missing = report.missing,
                                    complete = report.complete,
                                    pieces = report.piece_count,
                                    "recheck done"
                                );
                                let _ = events_p.send(ControlEvent::Rechecked {
                                    id,
                                    message,
                                    complete: report.complete,
                                    good: report.good,
                                    bad: report.bad,
                                    missing: report.missing,
                                    piece_count: report.piece_count,
                                });

                                // Sync hot set with catalog bitfield on mutate (ordered).
                                // Reload hot bitfield via ordered stop/start when needed.
                                let want_start = Catalog::open(&db_r)
                                    .ok()
                                    .and_then(|cat| cat.list_torrents().ok())
                                    .map(|rows| {
                                        rows.into_iter().any(|row| row.id == id && row.want_start)
                                    })
                                    .unwrap_or(false);
                                if was_hot || want_start {
                                    let _ = mut_tx_r.send(MutationJob::SyncAfterRecheck {
                                        id,
                                        start: was_hot || want_start,
                                    });
                                }
                            }
                            Err(e) => {
                                tracing::error!(
                                    id,
                                    torrent = %name_r,
                                    error = %e,
                                    "recheck failed"
                                );
                                let _ = events_p.send(ControlEvent::RecheckFailed {
                                    id,
                                    error: e.to_string(),
                                });
                            }
                        }
                    })
                {
                    session.end_recheck(id);
                    let _ = events.send(ControlEvent::RecheckFailed {
                        id,
                        error: format!("spawn recheck thread: {e}"),
                    });
                }
            }
        }
    }
}

fn torrent_name_hint(session: &SessionRuntime, id: i64) -> String {
    session
        .snapshot_nonblocking()
        .torrents
        .into_iter()
        .find(|t| t.id == id)
        .map(|t| t.name)
        .unwrap_or_else(|| "?".into())
}
