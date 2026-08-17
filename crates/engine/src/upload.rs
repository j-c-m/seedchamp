//! Seed upload I/O for peer sessions: [`begin_upload`] / [`write_framed_piece`] via
//! `OutQueue` (ctrl preferred over next PIECE).
//!
//! Backend selection: [`UploadOptions`] / [`ResolvedUploadBackend`]. Default
//! peer seed fill (`auto`): Linux Compio `read_at` on **ext4/xfs/btrfs** else
//! pread; Darwin **pread**; FreeBSD Compio. `compio` forces Compio on any FS.
//! Wire push is Compio `write_all`. RC4 fills then encrypts.
//! Request geometry / LTEP `reqq`: [`queue`].

pub mod queue;

mod fs_gate;
mod inflight;
mod read;

#[cfg(test)]
mod tests;

pub use inflight::{begin_upload, write_framed_piece, InFlightUpload};
pub use queue::{
    classify_upload_request, UploadBlock, UploadRequestStatus, MAX_REQUEST_LENGTH, MAX_UPLOAD_REQQ,
};

use crate::error::{Error, Result};

/// Config/CLI string for `[upload].backend` / `SEEDCHAMP_UPLOAD_BACKEND`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadBackend {
    /// Platform default: Linux FS-gated Compio; Darwin pread; FreeBSD Compio.
    Auto,
    /// Blocking `pread` on the peer worker (known stall during fill).
    Pread,
    /// Force Compio fill (any FS; no Linux FS gate).
    Compio,
}

impl UploadBackend {
    pub fn parse(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" | "" => Ok(Self::Auto),
            "pread" => Ok(Self::Pread),
            "compio" => Ok(Self::Compio),
            other => Err(Error::Msg(format!(
                "unknown upload.backend {other:?} (auto|pread|compio)"
            ))),
        }
    }

    /// Resolve config to runtime backend.
    pub fn resolve(self) -> Result<ResolvedUploadBackend> {
        match self {
            Self::Auto => Ok(ResolvedUploadBackend::Auto),
            Self::Pread => Ok(ResolvedUploadBackend::Pread),
            Self::Compio => Ok(ResolvedUploadBackend::Compio),
        }
    }
}

/// Runtime backend after resolution (stored on peer config).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ResolvedUploadBackend {
    /// Platform default fill path (see [`fs_gate`]).
    #[default]
    Auto,
    Pread,
    /// Always Compio `read_at` (from `compio`); no FS gate.
    Compio,
}

/// Peer upload I/O selection ([`ResolvedUploadBackend`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UploadOptions {
    pub backend: ResolvedUploadBackend,
}

impl Default for UploadOptions {
    fn default() -> Self {
        Self {
            backend: ResolvedUploadBackend::Auto,
        }
    }
}

/// BitTorrent PIECE frame header length (4 len + 1 id + 4 index + 4 begin).
pub const PIECE_HEADER_LEN: usize = 13;

/// Standard block size we put on the wire for upload (last block may be shorter).
pub const UPLOAD_BLOCK_LEN: u32 = 16 * 1024;

/// Fixed upload scratch: header + one standard block. Never grows.
pub const UPLOAD_SCRATCH_LEN: usize = PIECE_HEADER_LEN + UPLOAD_BLOCK_LEN as usize;
