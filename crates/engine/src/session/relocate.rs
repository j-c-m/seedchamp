//! Live payload relocate and leech_cache handoff.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

pub struct RelocateReport {
    pub kind: RelocateKind,
    /// Catalog / live payload root after the op (stage if retarget-only).
    pub data_root: PathBuf,
    pub note: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelocateKind {
    NoOp,
    /// Still on leech_cache stage; only `home_root` changed.
    RetargetHome,
    /// Payload files moved and `data_root` updated.
    Moved,
}

pub(super) struct SwitchPayloadOpts {
    /// After successful transfer + old-root cleanup, set `home_root` NULL.
    clear_home_root: bool,
}

impl super::SessionRuntime {
    /// If torrent is staged on `leech_cache` (`home_root` set), move to home then live-switch.
    ///
    /// **No stop/start.** Uses [`Self::switch_payload_root`] (hardlink/copy, then swap).
    pub async fn maybe_start_leech_cache_handoff(&self, id: i64) {
        let home = match self.with_catalog(|cat| cat.get_home_root(id)) {
            Ok(Some(h)) => h,
            _ => return,
        };
        let sess = self.clone();
        let home2 = home.clone();
        let result = crate::runtime::PeerWorkerPool::run_blocking(move || {
            sess.run_leech_cache_handoff(id, &home2)
        })
        .await;
        match result {
            Ok(Ok(())) => {
                tracing::info!(id, home = %home.display(), "leech_cache handoff complete");
                *self.inner.status.write() = format!("#{id} handoff → library complete — seeding");
            }
            Ok(Err(e)) => {
                tracing::warn!(id, error = %e, "leech_cache handoff failed");
                *self.inner.status.write() = format!("#{id} handoff failed: {e}");
            }
            Err(e) => {
                tracing::warn!(id, error = %e, "leech_cache handoff task join failed");
            }
        }
    }

    /// Blocking handoff without stopping peers (live layout swap).
    pub(super) fn run_leech_cache_handoff(&self, id: i64, home: &std::path::Path) -> Result<()> {
        let (from, complete) = self.payload_root_and_complete(id)?;
        if from == *home {
            let _ = self.with_catalog_mut(|cat| cat.set_home_root(id, None));
            return Ok(());
        }
        if !complete {
            return Ok(());
        }

        tracing::info!(
            id,
            stage = %from.display(),
            home = %home.display(),
            "leech_cache: moving to library (live seed; publish then swap)"
        );
        *self.inner.status.write() = format!("#{id} moving to library…");
        self.switch_payload_root(
            id,
            &from,
            home,
            SwitchPayloadOpts {
                clear_home_root: true,
            },
        )?;
        Ok(())
    }

    /// Live or offline relocate of payload `data_root` (Ctrl-O).
    ///
    /// - **Staged** (`home_root` set and payload still on stage): retarget `home_root` only.
    /// - **Hot:** publish dest + catalog + `set_data_root_live` + unpublish this
    ///   torrent's files (wipe tree only when `clear_home_root`, i.e. leech-cache stage).
    /// - **Cold:** catalog `relocate_torrent_data`.
    pub fn relocate_data_root(&self, id: i64, new_root: &Path) -> Result<RelocateReport> {
        let new_root = new_root.to_path_buf();
        if new_root.as_os_str().is_empty() {
            return Err(Error::Msg("relocate: empty path".into()));
        }

        // Still on leech_cache stage → only change permanent home destination.
        let home = self.with_catalog(|cat| cat.get_home_root(id))?;
        let data_root = self.payload_data_root(id)?;
        if let Some(ref h) = home {
            if data_root != *h {
                if *h == new_root {
                    return Ok(RelocateReport {
                        kind: RelocateKind::NoOp,
                        data_root,
                        note: "home_root unchanged (still staged)".into(),
                    });
                }
                self.with_catalog_mut(|cat| cat.set_home_root(id, Some(&new_root)))?;
                tracing::info!(
                    id,
                    home = %new_root.display(),
                    stage = %data_root.display(),
                    "retargeted home_root (still on leech_cache stage)"
                );
                return Ok(RelocateReport {
                    kind: RelocateKind::RetargetHome,
                    data_root,
                    note: format!("home → {} (still staging)", new_root.display()),
                });
            }
        }

        if data_root == new_root {
            return Ok(RelocateReport {
                kind: RelocateKind::NoOp,
                data_root,
                note: "data_root unchanged".into(),
            });
        }

        if self.is_hot(id) {
            *self.inner.status.write() = format!("#{id} relocating → {}…", new_root.display());
            let stats = self.switch_payload_root(
                id,
                &data_root,
                &new_root,
                SwitchPayloadOpts {
                    clear_home_root: false,
                },
            )?;
            tracing::info!(
                id,
                from = %data_root.display(),
                to = %new_root.display(),
                linked = stats.linked,
                copied = stats.copied,
                missing = stats.missing,
                "live relocate ok"
            );
            return Ok(RelocateReport {
                kind: RelocateKind::Moved,
                data_root: new_root.clone(),
                note: format!("data → {}", new_root.display()),
            });
        }

        // Cold catalog-only.
        self.with_catalog_mut(|cat| crate::disk::relocate_torrent_data(cat, id, &new_root))?;
        Ok(RelocateReport {
            kind: RelocateKind::Moved,
            data_root: new_root.clone(),
            note: format!("data → {}", new_root.display()),
        })
    }

    pub(super) fn payload_data_root(&self, id: i64) -> Result<PathBuf> {
        let reg = self.inner.registry.read();
        if let Some(t) = reg.get_id(id) {
            return Ok(t.layout().data_root.clone());
        }
        drop(reg);
        self.with_catalog(|cat| cat.get_data_root(id))
    }

    pub(super) fn payload_root_and_complete(&self, id: i64) -> Result<(PathBuf, bool)> {
        let reg = self.inner.registry.read();
        if let Some(t) = reg.get_id(id) {
            return Ok((t.layout().data_root.clone(), t.is_download_complete()));
        }
        drop(reg);
        let layout = self.with_catalog(|cat| cat.load_storage_layout(id))?;
        let complete = self
            .with_catalog(|cat| {
                cat.list_torrents().map(|rows| {
                    rows.into_iter()
                        .find(|r| r.id == id)
                        .map(|r| r.complete)
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false);
        Ok((layout.data_root, complete))
    }

    /// Publish dest (source stays) → catalog `data_root` → live layout → unpublish `from`.
    ///
    /// Lock order (deadlock-safe): registry read (snapshot) → drop → transfer (no
    /// locks) → catalog_mu → registry read + layout write → drop → delete → catalog.
    pub(super) fn switch_payload_root(
        &self,
        id: i64,
        from: &Path,
        to: &Path,
        opts: SwitchPayloadOpts,
    ) -> Result<crate::disk::TransferStats> {
        if from == to {
            if opts.clear_home_root {
                let _ = self.with_catalog_mut(|cat| cat.set_home_root(id, None));
            }
            return Ok(crate::disk::TransferStats::default());
        }

        let layout = {
            let reg = self.inner.registry.read();
            if let Some(t) = reg.get_id(id) {
                let mut lay = (*t.layout()).clone();
                lay.data_root = from.to_path_buf();
                lay
            } else {
                drop(reg);
                let mut lay = self.with_catalog(|cat| cat.load_storage_layout(id))?;
                lay.data_root = from.to_path_buf();
                lay
            }
        };

        let stats = crate::disk::transfer_payload_files(&layout, to)?;

        // Durable catalog first; keep home_root until cleanup when clearing.
        self.with_catalog_mut(|cat| cat.set_data_root(id, to))?;

        if let Some(t) = self.inner.registry.read().get_id(id) {
            let old = t.set_data_root_live(to.to_path_buf());
            debug_assert_eq!(old, *from);
        }

        if let Err(e) =
            crate::disk::unpublish_payload_files(&layout, from, to, opts.clear_home_root)
        {
            tracing::warn!(
                id,
                from = %from.display(),
                error = %e,
                "payload root cleanup failed (catalog already at new root)"
            );
            if opts.clear_home_root {
                // Leave home_root so the torrent stays marked staged (status / reserved).
                return Err(e);
            }
        }

        if opts.clear_home_root {
            self.with_catalog_mut(|cat| cat.set_home_root(id, None))?;
        }
        Ok(stats)
    }
}
