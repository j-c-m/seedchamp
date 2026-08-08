//! Positioned writes for verified pieces (verify-before-write leech path).

#[cfg(not(unix))]
use std::io::{Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::unix::fs::FileExt;

use std::fs::{self, OpenOptions};

use super::fd_cache::FdCache;
use super::spans::{IoSpan, StorageLayout};
use crate::error::{Error, Result};

/// Create data files (and parents) and set lengths from the torrent layout.
///
/// Skips **priority 0 (off)** files — partial multi‑file torrents with thousands
/// of disabled files must not mkdir/touch every path on start (major CPU/IO spike).
///
/// Avoids `set_len` when the file already has the correct size (preallocation of
/// multi‑GB files can stall the UI thread on some filesystems).
///
/// `priorities` overrides per-file priority when set (live TUI on/off); length must
/// match `layout.files` or is ignored and `layout.files[].priority` is used.
pub fn ensure_storage(layout: &StorageLayout) -> Result<()> {
    ensure_storage_with_priorities(layout, None)
}

/// Like [`ensure_storage`], using live priorities (e.g. after turning a file on).
pub fn ensure_storage_with_priorities(
    layout: &StorageLayout,
    priorities: Option<&[i32]>,
) -> Result<()> {
    for (i, f) in layout.files.iter().enumerate() {
        let prio = priorities
            .and_then(|p| p.get(i).copied())
            .unwrap_or(f.priority);
        if prio <= 0 {
            continue;
        }
        let path = layout.absolute(&f.path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| Error::Path(parent.to_path_buf(), e.to_string()))?;
        }
        if let Ok(meta) = fs::metadata(&path) {
            if meta.is_file() && meta.len() == f.size {
                continue;
            }
        }
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .read(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| Error::Path(path.clone(), e.to_string()))?;
        let cur = file.metadata().map(|m| m.len()).unwrap_or(0);
        if cur != f.size {
            file.set_len(f.size)
                .map_err(|e| Error::Path(path, e.to_string()))?;
        }
    }
    Ok(())
}

/// Write exactly `span.length` bytes from `buf` into the span (pwrite).
pub fn write_span(cache: &mut FdCache, span: &IoSpan, buf: &[u8]) -> Result<()> {
    let need = span.length as usize;
    if buf.len() < need {
        return Err(Error::Msg("write buffer too small for span".into()));
    }
    let file = cache.open_write(&span.path)?;
    write_at_exact(file, span.file_offset, &buf[..need])?;
    Ok(())
}

/// Write a full verified piece to disk (no hash check here — caller must verify).
pub fn write_piece(
    cache: &mut FdCache,
    layout: &StorageLayout,
    index: u32,
    data: &[u8],
) -> Result<()> {
    let plen = layout.piece_size(index)? as usize;
    if data.len() != plen {
        return Err(Error::Msg(format!(
            "piece {index} length {} != expected {plen}",
            data.len()
        )));
    }
    let spans = layout.spans_for_piece(index)?;
    let mut off = 0usize;
    for span in &spans {
        let n = span.length as usize;
        write_span(cache, span, &data[off..off + n])?;
        off += n;
    }
    debug_assert_eq!(off, plen);
    Ok(())
}

#[cfg(unix)]
fn write_at_exact(file: &std::fs::File, offset: u64, buf: &[u8]) -> Result<()> {
    let mut done = 0usize;
    while done < buf.len() {
        match file.write_at(&buf[done..], offset + done as u64) {
            Ok(0) => {
                return Err(Error::Msg(format!(
                    "short write at offset {} (wrote {done}/{})",
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
fn write_at_exact(file: &std::fs::File, offset: u64, buf: &[u8]) -> Result<()> {
    let mut f = file.try_clone()?;
    f.seek(SeekFrom::Start(offset))?;
    f.write_all(buf)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk::read::read_piece;
    use crate::disk::spans::FileLayout;
    use std::path::PathBuf;

    #[test]
    fn write_then_read_piece() {
        let dir = tempfile::tempdir().unwrap();
        let layout = StorageLayout {
            data_root: dir.path().to_path_buf(),
            piece_length: 32,
            piece_count: 2,
            total_size: 48,
            files: vec![FileLayout {
                path: PathBuf::from("f.bin"),
                size: 48,
                offset: 0,
                priority: 1,
            }],
        };
        ensure_storage(&layout).unwrap();
        let mut cache = FdCache::default_cache();
        let data: Vec<u8> = (0..32).collect();
        write_piece(&mut cache, &layout, 0, &data).unwrap();
        let mut out = Vec::new();
        read_piece(&mut cache, &layout, 0, &mut out).unwrap();
        assert_eq!(out, data);
    }
}
