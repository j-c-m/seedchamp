//! Blocking `pread` via fd cache.
//!
//! Hash/recheck workers and the upload `pread` backend (peer-worker TLS cache).
//! Compio seed fill is [`crate::upload::begin_upload`]. Durable writes are
//! [`crate::runtime::DiskWorker`].

#[cfg(not(unix))]
use std::io::{Read, Seek, SeekFrom};
#[cfg(unix)]
use std::os::unix::fs::FileExt;

use super::fd_cache::FdCache;
use super::spans::{IoSpan, StorageLayout};
use crate::error::{Error, Result};

/// Default recheck / hash window (design: 256 KiB).
pub const HASH_READ_WINDOW: usize = 256 * 1024;

/// Blocking `pread`: read exactly `span.length` bytes into `buf`.
pub fn read_span(cache: &mut FdCache, span: &IoSpan, buf: &mut [u8]) -> Result<()> {
    let need = span.length as usize;
    if buf.len() < need {
        return Err(Error::Msg("read buffer too small for span".into()));
    }
    let file = cache.open_read(&span.path)?;
    read_at_exact(file, span.file_offset, &mut buf[..need])?;
    Ok(())
}

/// Read a full piece into `out` (resized).
pub fn read_piece(
    cache: &mut FdCache,
    layout: &StorageLayout,
    index: u32,
    out: &mut Vec<u8>,
) -> Result<()> {
    let plen = layout.piece_size(index)? as usize;
    out.resize(plen, 0);
    let spans = layout.spans_for_piece(index)?;
    let mut off = 0usize;
    for span in &spans {
        let n = span.length as usize;
        read_span(cache, span, &mut out[off..off + n])?;
        off += n;
    }
    debug_assert_eq!(off, plen);
    Ok(())
}

/// Hash a piece by streaming `HASH_READ_WINDOW`-sized reads (no full-piece requirement beyond piece size).
/// For pieces ≤ window, one shot; larger pieces loop windows across spans.
pub fn hash_piece_windowed(
    cache: &mut FdCache,
    layout: &StorageLayout,
    index: u32,
    hasher: &mut impl sha1::Digest,
) -> Result<()> {
    let plen = layout.piece_size(index)? as u64;
    if plen == 0 {
        return Ok(());
    }
    let start = index as u64 * layout.piece_length as u64;
    let mut remaining = plen;
    let mut torrent_off = start;
    let mut buf = vec![0u8; HASH_READ_WINDOW.min(plen as usize).max(1)];

    while remaining > 0 {
        let chunk = remaining.min(HASH_READ_WINDOW as u64);
        let spans = layout.spans_for_range(torrent_off, chunk)?;
        let mut filled = 0usize;
        for span in &spans {
            let n = span.length as usize;
            read_span(cache, span, &mut buf[filled..filled + n])?;
            filled += n;
        }
        hasher.update(&buf[..filled]);
        torrent_off += chunk;
        remaining -= chunk;
    }
    Ok(())
}

#[cfg(unix)]
fn read_at_exact(file: &std::fs::File, offset: u64, buf: &mut [u8]) -> Result<()> {
    let mut done = 0usize;
    while done < buf.len() {
        match file.read_at(&mut buf[done..], offset + done as u64) {
            Ok(0) => {
                return Err(Error::Msg(format!(
                    "short read at offset {} (got {done}/{})",
                    offset,
                    buf.len()
                )));
            }
            Ok(n) => done += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn read_at_exact(file: &std::fs::File, offset: u64, buf: &mut [u8]) -> Result<()> {
    let mut f = file.try_clone()?;
    f.seek(SeekFrom::Start(offset))?;
    f.read_exact(buf)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk::spans::FileLayout;
    use sha1::{Digest, Sha1};
    use std::io::Write;
    use std::path::PathBuf;

    #[test]
    fn read_and_hash_piece() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.bin");
        let data: Vec<u8> = (0u8..100).collect();
        std::fs::File::create(&path)
            .unwrap()
            .write_all(&data)
            .unwrap();

        let layout = StorageLayout {
            data_root: dir.path().to_path_buf(),
            piece_length: 64,
            piece_count: 2,
            total_size: 100,
            files: vec![FileLayout {
                path: PathBuf::from("a.bin"),
                size: 100,
                offset: 0,
                priority: 1,
            }],
        };

        let mut cache = FdCache::default_cache();
        let mut buf = Vec::new();
        read_piece(&mut cache, &layout, 0, &mut buf).unwrap();
        assert_eq!(&buf[..], &data[..64]);

        let mut h = Sha1::new();
        hash_piece_windowed(&mut cache, &layout, 0, &mut h).unwrap();
        let d = h.finalize();
        let mut h2 = Sha1::new();
        h2.update(&data[..64]);
        assert_eq!(d.as_slice(), h2.finalize().as_slice());
    }
}
