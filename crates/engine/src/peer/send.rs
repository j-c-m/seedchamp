//! Per-peer outbound send state (queue + write scratch).
//!
//! Seed fill uses the peer-worker thread-local [`crate::disk::FdCache`] (shared
//! by all peers on that `seedchamp-io` thread).

use std::time::Instant;

use compio::net::TcpStream;

use crate::crypto::Rc4;
use crate::error::Result;
use crate::upload::UPLOAD_SCRATCH_LEN;

use super::config::PeerConfig;
use super::ctrl_scratch::CtrlScratch;
use super::out_queue::{OutQueue, SendOutcome};
use crate::hot::HotTorrent;

/// Outbound half of a peer session: ordered queue + write scratch.
///
/// Download/request state lives on [`super::download::PeerDownload`].
pub(crate) struct PeerSend {
    pub out: OutQueue,
    pub scratch: Vec<u8>,
    /// Reused encode buffer for HAVE / Reject / KeepAlive.
    pub ctrl: CtrlScratch,
    /// Last time any byte was written to the socket (KeepAlive silence).
    pub last_send_at: Instant,
}

impl PeerSend {
    pub(crate) fn new() -> Self {
        Self {
            out: OutQueue::new(),
            scratch: vec![0u8; UPLOAD_SCRATCH_LEN],
            ctrl: CtrlScratch::new(),
            last_send_at: Instant::now(),
        }
    }

    /// Compio write path (+ peer-worker TLS fill); updates [`Self::last_send_at`].
    pub(crate) async fn pump(
        &mut self,
        stream: &mut TcpStream,
        torrent: &HotTorrent,
        cfg: &PeerConfig,
        encrypt: Option<&mut Rc4>,
        allow_upload: bool,
        fast_enabled: bool,
    ) -> Result<SendOutcome> {
        let outcome = self
            .out
            .send_quantum(
                stream,
                torrent,
                cfg,
                encrypt,
                &mut self.scratch,
                &mut self.ctrl,
                allow_upload,
                fast_enabled,
            )
            .await?;
        if outcome.wrote {
            self.last_send_at = Instant::now();
        }
        Ok(outcome)
    }
}
