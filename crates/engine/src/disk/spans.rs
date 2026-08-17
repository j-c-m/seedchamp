//! Map torrent byte ranges / pieces onto file spans.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// One contiguous read/write region in a single file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IoSpan {
    pub path: PathBuf,
    pub file_offset: u64,
    pub length: u64,
}

/// File layout entry (torrent stream coordinates).
#[derive(Debug, Clone)]
pub struct FileLayout {
    pub path: PathBuf,
    pub size: u64,
    pub offset: u64,
    /// Download priority: **0 = off** (do not download), **≥1 = on** (normal).
    pub priority: i32,
}

impl FileLayout {
    pub fn end(&self) -> u64 {
        self.offset.saturating_add(self.size)
    }

    pub fn wanted(&self) -> bool {
        self.priority > 0
    }
}

/// Storage layout for a torrent on disk.
#[derive(Debug, Clone)]
pub struct StorageLayout {
    pub data_root: PathBuf,
    pub piece_length: u32,
    pub piece_count: u32,
    pub total_size: u64,
    pub files: Vec<FileLayout>,
}

impl StorageLayout {
    /// Absolute path for a relative torrent file path.
    pub fn absolute(&self, rel: &Path) -> PathBuf {
        self.data_root.join(rel)
    }

    /// Byte length of piece `index` (last piece may be shorter).
    pub fn piece_size(&self, index: u32) -> Result<u32> {
        if index >= self.piece_count {
            return Err(Error::Msg(format!(
                "piece index {index} out of range (count {})",
                self.piece_count
            )));
        }
        let start = index as u64 * self.piece_length as u64;
        if start >= self.total_size {
            return Ok(0);
        }
        let end = (start + self.piece_length as u64).min(self.total_size);
        Ok((end - start) as u32)
    }

    /// Map a torrent-stream byte range to file spans.
    pub fn spans_for_range(&self, range_start: u64, range_len: u64) -> Result<Vec<IoSpan>> {
        if range_len == 0 {
            return Ok(Vec::new());
        }
        let range_end = range_start.saturating_add(range_len);
        if range_end > self.total_size {
            return Err(Error::Msg(format!(
                "range {range_start}+{range_len} exceeds total_size {}",
                self.total_size
            )));
        }

        let mut spans = Vec::new();
        let mut pos = range_start;

        while pos < range_end {
            let file = self
                .files
                .iter()
                .find(|f| pos >= f.offset && pos < f.end())
                .ok_or_else(|| Error::Msg(format!("no file covers torrent offset {pos}")))?;

            let into_file = pos - file.offset;
            let avail = file.size - into_file;
            let take = avail.min(range_end - pos);

            spans.push(IoSpan {
                path: self.absolute(&file.path),
                file_offset: into_file,
                length: take,
            });
            pos += take;
        }

        Ok(spans)
    }

    /// Spans covering piece `index`.
    pub fn spans_for_piece(&self, index: u32) -> Result<Vec<IoSpan>> {
        let len = self.piece_size(index)? as u64;
        let start = index as u64 * self.piece_length as u64;
        self.spans_for_range(start, len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout_multi() -> StorageLayout {
        // file0: 100 bytes @ 0, file1: 50 @ 100; piece_length 64
        StorageLayout {
            data_root: PathBuf::from("/data"),
            piece_length: 64,
            piece_count: 3, // 150 bytes → 3 pieces (64+64+22)
            total_size: 150,
            files: vec![
                FileLayout {
                    path: PathBuf::from("a.bin"),
                    size: 100,
                    offset: 0,
                    priority: 1,
                },
                FileLayout {
                    path: PathBuf::from("b.bin"),
                    size: 50,
                    offset: 100,
                    priority: 1,
                },
            ],
        }
    }

    #[test]
    fn single_file_piece() {
        let lay = StorageLayout {
            data_root: PathBuf::from("/x"),
            piece_length: 32,
            piece_count: 1,
            total_size: 10,
            files: vec![FileLayout {
                path: PathBuf::from("f"),
                size: 10,
                offset: 0,
                priority: 1,
            }],
        };
        let s = lay.spans_for_piece(0).unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].file_offset, 0);
        assert_eq!(s[0].length, 10);
        assert_eq!(s[0].path, PathBuf::from("/x/f"));
    }

    #[test]
    fn multi_file_crosses_boundary() {
        let lay = layout_multi();
        // piece 1: [64, 128) — 36 bytes in a, 28 in b
        let s = lay.spans_for_piece(1).unwrap();
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].path, PathBuf::from("/data/a.bin"));
        assert_eq!(s[0].file_offset, 64);
        assert_eq!(s[0].length, 36);
        assert_eq!(s[1].path, PathBuf::from("/data/b.bin"));
        assert_eq!(s[1].file_offset, 0);
        assert_eq!(s[1].length, 28);
    }

    #[test]
    fn last_piece_short() {
        let lay = layout_multi();
        let s = lay.spans_for_piece(2).unwrap();
        assert_eq!(lay.piece_size(2).unwrap(), 22);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].file_offset, 28);
        assert_eq!(s[0].length, 22);
    }
}
