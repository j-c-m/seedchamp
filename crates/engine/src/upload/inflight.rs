//! In-progress PIECE send: fill ([`begin_upload`]) and wire push ([`write_framed_piece`]).

use compio::buf::{BufResult, IntoInner, IoBuf};
use compio::net::TcpStream;

use crate::crypto::Rc4;
use crate::disk::{open_read_compio_peer, with_peer_fd_cache, StorageLayout};
use crate::error::{Error, Result};

use super::fs_gate::prefer_compio_fill;
use super::queue::UploadBlock;
use super::read::fill_piece_header;
use super::{UploadOptions, PIECE_HEADER_LEN, UPLOAD_BLOCK_LEN, UPLOAD_SCRATCH_LEN};

/// In-progress PIECE send (header+payload in `scratch`, written with Compio `write_all`).
pub struct InFlightUpload {
    pub block: UploadBlock,
    /// RC4 has already advanced for this message (scratch holds ciphertext).
    /// Dropping the active send would desync the peer's decrypt stream.
    cipher_applied: bool,
    /// Upload rate-limit tokens reserved for this block (0 if unlimited / not reserved).
    pub rate_reserved: u64,
    pub(crate) state: InFlightState,
}

pub(crate) enum InFlightState {
    /// Payload in peer `scratch[0..total]`; Compio `write_all` until done.
    Buffered {
        total: usize,
        sent: usize,
        payload: u64,
    },
}

impl InFlightUpload {
    /// True if any byte of this PIECE message has been written (cannot Cancel cleanly).
    pub fn any_wire_bytes(&self) -> bool {
        match &self.state {
            InFlightState::Buffered { sent, .. } => *sent > 0,
        }
    }

    /// True when the send may be dropped without partial wire bytes or RC4 desync.
    #[inline]
    pub fn can_abort(&self) -> bool {
        !self.any_wire_bytes() && !self.cipher_applied
    }

    #[cfg(test)]
    pub(crate) fn test_buffered(
        block: UploadBlock,
        cipher_applied: bool,
        sent: usize,
        total: usize,
    ) -> Self {
        Self {
            block,
            cipher_applied,
            rate_reserved: 0,
            state: InFlightState::Buffered {
                total: total.max(sent).max(1),
                sent,
                payload: 0,
            },
        }
    }
}

/// Fill `scratch` with PIECE header+payload.
///
/// - [`ResolvedUploadBackend::Pread`]: blocking `pread`.
/// - [`ResolvedUploadBackend::Auto`]: Linux FS-gated Compio (ext4/xfs/btrfs) else pread;
///   Darwin pread; FreeBSD Compio.
/// - [`ResolvedUploadBackend::Compio`]: always Compio (`compio`).
pub async fn begin_upload(
    layout: &StorageLayout,
    block: UploadBlock,
    rc4: Option<&mut Rc4>,
    opts: UploadOptions,
    scratch: &mut Vec<u8>,
) -> Result<InFlightUpload> {
    let UploadBlock {
        index,
        begin,
        length,
    } = block;
    let plen = layout.piece_size(index)?;
    if begin as u64 + length as u64 > plen as u64 {
        return Err(Error::Msg("request past piece end".into()));
    }
    if length == 0 {
        return Err(Error::Msg("zero-length piece request".into()));
    }
    if length > UPLOAD_BLOCK_LEN {
        return Err(Error::Msg(format!(
            "piece request length {length} > upload block {UPLOAD_BLOCK_LEN}"
        )));
    }

    let torrent_off = index as u64 * layout.piece_length as u64 + begin as u64;
    let spans = layout.spans_for_range(torrent_off, length as u64)?;

    debug_assert!(
        scratch.len() >= UPLOAD_SCRATCH_LEN,
        "upload scratch must be UPLOAD_SCRATCH_LEN"
    );

    let total = PIECE_HEADER_LEN + length as usize;
    fill_piece_header(&mut scratch[..PIECE_HEADER_LEN], index, begin, length);

    if prefer_compio_fill(opts.backend, &spans) {
        fill_payload_compio(&spans, scratch, length as usize).await?;
    } else {
        fill_payload_pread(&spans, &mut scratch[PIECE_HEADER_LEN..total])?;
    }

    let mut cipher_applied = false;
    if let Some(c) = rc4 {
        c.crypt_inplace(&mut scratch[..PIECE_HEADER_LEN]);
        c.crypt_inplace(&mut scratch[PIECE_HEADER_LEN..total]);
        cipher_applied = true;
    }
    Ok(InFlightUpload {
        block,
        cipher_applied,
        rate_reserved: 0,
        state: InFlightState::Buffered {
            total,
            sent: 0,
            payload: length as u64,
        },
    })
}

/// Blocking multi-span `pread` via peer-worker TLS cache (stalls the peer task).
fn fill_payload_pread(spans: &[crate::disk::spans::IoSpan], out: &mut [u8]) -> Result<()> {
    let mut filled = 0usize;
    for span in spans {
        let n = span.length as usize;
        with_peer_fd_cache(|cache| {
            crate::disk::read_span_blocking(cache, span, &mut out[filled..filled + n])
        })?;
        filled += n;
    }
    Ok(())
}

/// Positioned multi-span fill via Compio completion I/O **into** `scratch`
/// (no per-span heap buffer / memcpy).
///
/// Payload lands at `scratch[PIECE_HEADER_LEN..PIECE_HEADER_LEN+total_len]`.
/// Open uses the peer-worker TLS cache (short borrow); `read_exact_at` is outside.
#[allow(unsafe_code)] // Vec::set_len on upload scratch capacity only
async fn fill_payload_compio(
    spans: &[crate::disk::spans::IoSpan],
    scratch: &mut Vec<u8>,
    total_len: usize,
) -> Result<()> {
    use compio::io::AsyncReadAtExt;

    let want_len = scratch.len();
    let payload_end = PIECE_HEADER_LEN + total_len;
    if payload_end > want_len || want_len < UPLOAD_SCRATCH_LEN {
        return Err(Error::Msg("compio fill buffer length mismatch".into()));
    }

    let mut filled = 0usize;
    for span in spans {
        let n = span.length as usize;
        let start = PIECE_HEADER_LEN + filled;
        let end = start + n;
        if end > payload_end {
            return Err(Error::Msg("compio fill span past payload end".into()));
        }
        let file = open_read_compio_peer(&span.path).await?;
        let mut buf = std::mem::take(scratch);
        // SAFETY: start <= want_len; [0..start) holds header (+ prior spans).
        unsafe {
            buf.set_len(start);
        }
        let slice = buf.slice(start..end);
        let BufResult(res, slice) = file.read_exact_at(slice, span.file_offset).await;
        let mut buf = slice.into_inner();
        // SAFETY: allocation is still the full upload scratch.
        unsafe {
            buf.set_len(want_len);
        }
        *scratch = buf;
        res.map_err(|e| Error::Msg(format!("compio read_at {}: {e}", span.path.display())))?;
        filled += n;
    }
    if filled != total_len {
        return Err(Error::Msg(format!(
            "compio fill short {filled}/{total_len}"
        )));
    }
    Ok(())
}

/// Write the full PIECE via Compio `write_all` of the peer upload scratch prefix
/// (owned IoBuf — no per-write clone, no truncate/resize of the scratch).
///
/// Returns payload byte count (not counting the 13-byte header).
pub async fn write_framed_piece(
    stream: &mut TcpStream,
    inflight: &mut InFlightUpload,
    scratch: &mut Vec<u8>,
) -> Result<u64> {
    match &mut inflight.state {
        InFlightState::Buffered {
            total,
            sent,
            payload,
        } => {
            if *sent < *total {
                // Full frame is always written in one op (sent stays 0 until done).
                debug_assert_eq!(*sent, 0);
                let total = *total;
                debug_assert!(
                    scratch.len() >= total && scratch.len() >= UPLOAD_SCRATCH_LEN,
                    "upload scratch must hold framed PIECE"
                );
                let buf = std::mem::take(scratch);
                match crate::net::write_all_owned_prefix(stream, buf, total).await {
                    Ok(buf) => {
                        *scratch = buf;
                        *sent = total;
                    }
                    Err((e, buf)) => {
                        *scratch = buf;
                        return Err(e);
                    }
                }
            }
            Ok(*payload)
        }
    }
}
