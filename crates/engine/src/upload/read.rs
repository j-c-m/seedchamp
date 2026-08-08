//! Block reads and PIECE header layout for seed upload.

use crate::disk::{FdCache, StorageLayout};
use crate::error::{Error, Result};

use super::PIECE_HEADER_LEN;

/// Compio positioned read of piece block `[begin, begin+length)` into `out`.
pub async fn read_block_into(
    cache: &mut FdCache,
    layout: &StorageLayout,
    index: u32,
    begin: u32,
    length: u32,
    out: &mut [u8],
) -> Result<()> {
    if out.len() != length as usize {
        return Err(Error::Msg("read_block_into buffer length mismatch".into()));
    }
    let plen = layout.piece_size(index)?;
    if begin as u64 + length as u64 > plen as u64 {
        return Err(Error::Msg("request past piece end".into()));
    }
    let torrent_off = index as u64 * layout.piece_length as u64 + begin as u64;
    let spans = layout.spans_for_range(torrent_off, length as u64)?;
    fill_spans_compio(cache, &spans, out).await
}

/// Compio read into a reusable `Vec` (resized to `length`).
pub async fn read_block(
    cache: &mut FdCache,
    layout: &StorageLayout,
    index: u32,
    begin: u32,
    length: u32,
    out: &mut Vec<u8>,
) -> Result<()> {
    out.clear();
    out.resize(length as usize, 0);
    read_block_into(cache, layout, index, begin, length, out.as_mut_slice()).await
}

async fn fill_spans_compio(
    cache: &mut FdCache,
    spans: &[crate::disk::spans::IoSpan],
    out: &mut [u8],
) -> Result<()> {
    use compio::buf::BufResult;
    use compio::io::AsyncReadAtExt;

    let mut filled = 0usize;
    for span in spans {
        let n = span.length as usize;
        let file = cache.open_read_compio(&span.path).await?;
        let chunk = vec![0u8; n];
        let BufResult(res, chunk) = file.read_exact_at(chunk, span.file_offset).await;
        res.map_err(|e| Error::Msg(format!("compio read_at {}: {e}", span.path.display())))?;
        out[filled..filled + n].copy_from_slice(&chunk[..n]);
        filled += n;
    }
    Ok(())
}

pub(crate) fn fill_piece_header(dst: &mut [u8], index: u32, begin: u32, length: u32) {
    debug_assert_eq!(dst.len(), PIECE_HEADER_LEN);
    let msg_len = 9 + length;
    dst[0..4].copy_from_slice(&msg_len.to_be_bytes());
    dst[4] = 7u8; // PIECE
    dst[5..9].copy_from_slice(&index.to_be_bytes());
    dst[9..13].copy_from_slice(&begin.to_be_bytes());
}

#[cfg(test)]
pub(crate) fn build_piece_header(index: u32, begin: u32, length: u32) -> [u8; PIECE_HEADER_LEN] {
    let mut header = [0u8; PIECE_HEADER_LEN];
    fill_piece_header(&mut header, index, begin, length);
    header
}
