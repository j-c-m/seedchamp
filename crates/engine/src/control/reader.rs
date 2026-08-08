//! Catalog **read-only** worker (TUI list and future RO queries).
//!
//! Owns a dedicated SQLite connection with a short busy timeout so long scans
//! never sit on the mutation worker or the control command loop.

use std::path::PathBuf;
use std::sync::mpsc;

use crate::catalog::Catalog;

use super::types::{CatalogReadJob, ControlEvent};

/// RO catalog jobs for [`catalog_reader_worker`].
pub(super) fn catalog_reader_worker(
    db: PathBuf,
    rx: mpsc::Receiver<CatalogReadJob>,
    events: mpsc::Sender<ControlEvent>,
) {
    let mut cat: Option<Catalog> = None;
    while let Ok(job) = rx.recv() {
        match job {
            CatalogReadJob::Shutdown => break,
            CatalogReadJob::ListCatalog { filter: filter0 } => {
                // Coalesce: only the latest filter in the queue matters.
                let mut filter = filter0;
                while let Ok(more) = rx.try_recv() {
                    match more {
                        CatalogReadJob::Shutdown => return,
                        CatalogReadJob::ListCatalog { filter: f } => filter = f,
                    }
                }

                if cat.is_none() {
                    cat = Catalog::open_for_ui(&db).ok();
                }
                let Some(c) = cat.as_mut() else {
                    let _ = events.send(ControlEvent::CatalogListFailed {
                        filter,
                        error: "catalog open failed".into(),
                    });
                    continue;
                };

                let filt = if filter.is_empty() {
                    None
                } else {
                    Some(filter.as_str())
                };
                match c.list_torrents_filtered(filt) {
                    Ok(rows) => match c.session_limits() {
                        Ok(limits) => {
                            let _ = events.send(ControlEvent::CatalogList {
                                filter,
                                rows,
                                limits,
                            });
                        }
                        Err(e) => {
                            cat = None;
                            let _ = events.send(ControlEvent::CatalogListFailed {
                                filter,
                                error: e.to_string(),
                            });
                        }
                    },
                    Err(e) => {
                        cat = None;
                        let _ = events.send(ControlEvent::CatalogListFailed {
                            filter,
                            error: e.to_string(),
                        });
                    }
                }
            }
        }
    }
}
