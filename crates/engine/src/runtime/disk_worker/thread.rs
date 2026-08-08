//! Sync `pwrite` backend on a dedicated `seedchamp-disk` thread (portable fallback).

use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use crate::disk::FdCache;
use crate::error::{Error, Result};

use super::{
    complete_discard_job, complete_write_job, write_job_sync, DiskWriteBackend, DiskWriteJob,
};

pub struct ThreadBackend {
    tx: SyncSender<DiskWriteJob>,
    _guard: Arc<ThreadGuard>,
}

struct ThreadGuard {
    join: std::sync::Mutex<Option<JoinHandle<()>>>,
}

impl Drop for ThreadGuard {
    fn drop(&mut self) {
        if let Ok(mut g) = self.join.lock() {
            if let Some(h) = g.take() {
                let _ = h.join();
            }
        }
    }
}

impl ThreadBackend {
    pub fn spawn(depth: usize, discard_writes: bool) -> Result<Self> {
        let (tx, rx) = mpsc::sync_channel::<DiskWriteJob>(depth.max(1));
        let join = thread::Builder::new()
            .name("seedchamp-disk".into())
            .spawn(move || thread_main(rx, discard_writes))
            .map_err(|e| Error::Msg(format!("spawn disk thread: {e}")))?;
        Ok(Self {
            tx,
            _guard: Arc::new(ThreadGuard {
                join: std::sync::Mutex::new(Some(join)),
            }),
        })
    }
}

impl DiskWriteBackend for ThreadBackend {
    fn submit(&self, job: DiskWriteJob) -> std::result::Result<(), (Error, DiskWriteJob)> {
        self.tx
            .send(job)
            .map_err(|e| (Error::DiskWorkerStopped, e.0))
    }
}

fn thread_main(rx: Receiver<DiskWriteJob>, discard_writes: bool) {
    let mut cache = FdCache::default_cache();
    while let Ok(job) = rx.recv() {
        if discard_writes {
            complete_discard_job(job);
        } else {
            let r = write_job_sync(&mut cache, &job);
            complete_write_job(job, r);
        }
    }
    tracing::debug!("disk thread exit");
}
