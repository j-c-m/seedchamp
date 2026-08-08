//! Parse .torrent metainfo into structured fields + infohash.

use std::path::{Path, PathBuf};

use sha1::{Digest, Sha1};

use crate::bencode::{self, Value};
use crate::error::{Error, Result};

/// One file in the torrent.
#[derive(Debug, Clone)]
pub struct TorrentFile {
    pub path: PathBuf,
    pub size: u64,
    pub offset: u64,
}

/// Parsed metainfo (from .torrent bytes).
#[derive(Debug, Clone)]
pub struct Metainfo {
    pub infohash: [u8; 20],
    pub name: String,
    pub piece_length: u32,
    pub piece_count: u32,
    pub total_size: u64,
    pub pieces: Vec<u8>,
    pub files: Vec<TorrentFile>,
    /// True when the torrent used the multi-file `info.files` form (BEP 3).
    /// On disk, files live under `data_root / name / …`.
    pub is_multi_file: bool,
    pub private: bool,
    pub trackers: Vec<(u32, String)>, // (tier, url)
    pub announce: Option<String>,
}

impl Metainfo {
    pub fn infohash_hex(&self) -> String {
        hex::encode(self.infohash)
    }

    pub fn parse_file(path: &Path) -> Result<Self> {
        let bytes =
            std::fs::read(path).map_err(|e| Error::Path(path.to_path_buf(), e.to_string()))?;
        Self::parse_bytes(&bytes)
    }

    pub fn parse_bytes(data: &[u8]) -> Result<Self> {
        let root = bencode::decode_full(data)?;
        let dict = root
            .as_dict()
            .ok_or_else(|| Error::Metainfo("root is not a dict".into()))?;

        let info_raw = bencode::find_raw_dict_value(data, b"info")?;
        let mut hasher = Sha1::new();
        hasher.update(info_raw);
        let digest = hasher.finalize();
        let mut infohash = [0u8; 20];
        infohash.copy_from_slice(&digest);

        let info = root
            .dict_get("info")
            .ok_or_else(|| Error::Metainfo("missing info".into()))?;

        let name = info.dict_get_str("name").unwrap_or("unknown").to_string();

        let piece_length =
            info.dict_get_int("piece length")
                .ok_or_else(|| Error::Metainfo("missing piece length".into()))? as u32;

        if piece_length == 0 {
            return Err(Error::Metainfo("piece length is zero".into()));
        }

        let pieces = info
            .dict_get_bytes("pieces")
            .ok_or_else(|| Error::Metainfo("missing pieces".into()))?
            .to_vec();

        if pieces.len() % 20 != 0 {
            return Err(Error::Metainfo("pieces length not multiple of 20".into()));
        }
        let piece_count = (pieces.len() / 20) as u32;

        let private = info.dict_get_int("private").unwrap_or(0) != 0;

        let is_multi_file = info.dict_get_list("files").is_some();
        let files = parse_files(info)?;
        let total_size: u64 = files.iter().map(|f| f.size).sum();

        let expected_pieces = total_size.div_ceil(piece_length as u64) as u32;
        if total_size > 0 && expected_pieces != piece_count {
            // Allow trailing empty edge; still warn via error for hard mismatch
            if expected_pieces != piece_count {
                return Err(Error::Metainfo(format!(
                    "piece count mismatch: pieces blob has {piece_count}, size implies {expected_pieces}"
                )));
            }
        }

        let mut trackers = Vec::new();
        if let Some(list) = root.dict_get_list("announce-list") {
            for (tier, entry) in list.iter().enumerate() {
                if let Some(urls) = entry.as_list() {
                    for u in urls {
                        if let Some(s) = u.as_str() {
                            trackers.push((tier as u32, s.to_string()));
                        }
                    }
                }
            }
        }
        let announce = root.dict_get_str("announce").map(|s| s.to_string());
        if trackers.is_empty() {
            if let Some(ref a) = announce {
                trackers.push((0, a.clone()));
            }
        }

        let _ = dict; // silence if unused

        Ok(Metainfo {
            infohash,
            name,
            piece_length,
            piece_count,
            total_size,
            pieces,
            files,
            is_multi_file,
            private,
            trackers,
            announce,
        })
    }
}

/// Max bytes per path component (common `NAME_MAX` on Unix; keeps paths portable).
const MAX_COMPONENT_BYTES: usize = 255;

/// Normalize one path component for on-disk use under `data_root`.
///
/// **Security:** rejects empty / `.` / `..` / separators / NUL after cleanup.
/// **Interop:** keeps spaces, colons, brackets, etc. (valid on FreeBSD/Linux and
/// matches rtorrent’s usual root dir). Control chars and other separators become `_`.
///
/// For watch-template placeholders (`{torrent_name}`) see the stricter
/// [`crate::watch::sanitize_path_component`].
pub fn normalize_path_component(s: &str) -> Result<String> {
    let mut out = String::with_capacity(s.len());
    let mut prev_us = false;
    for ch in s.chars() {
        // Separators and NUL must never appear in a component.
        let hard_bad = matches!(ch, '/' | '\\' | '\0');
        // Controls are awkward in shells/logs; replace rather than reject the torrent.
        let soft_bad = ch.is_control();
        if hard_bad || soft_bad {
            if !prev_us && !out.is_empty() {
                out.push('_');
                prev_us = true;
            }
            continue;
        }
        out.push(ch);
        prev_us = false;
    }
    // Trim trailing dots/spaces (problematic on some FS / copy-to-Windows).
    let out = out
        .trim_matches(|c: char| c == '.' || c == ' ' || c == '\t')
        .to_string();
    if out.is_empty() || out == "." || out == ".." {
        return Err(Error::Metainfo(format!(
            "unsafe or empty path component after normalize: {s:?}"
        )));
    }
    // Byte-length cap (UTF-8); keep character boundary.
    if out.len() > MAX_COMPONENT_BYTES {
        let mut cut = MAX_COMPONENT_BYTES;
        while cut > 0 && !out.is_char_boundary(cut) {
            cut -= 1;
        }
        return Ok(out[..cut].trim_end_matches(['.', ' ', '_']).to_string());
    }
    Ok(out)
}

fn parse_files(info: &Value) -> Result<Vec<TorrentFile>> {
    let raw_name = info.dict_get_str("name").unwrap_or("unknown");
    let torrent_name = normalize_path_component(raw_name).unwrap_or_else(|_| "unnamed".into());

    // Multi-file (BEP 3): on-disk layout is `name/path/to/file` under data_root.
    if let Some(list) = info.dict_get_list("files") {
        let mut out = Vec::new();
        let mut offset = 0u64;
        for (idx, f) in list.iter().enumerate() {
            let length = f
                .dict_get_int("length")
                .ok_or_else(|| Error::Metainfo(format!("files[{idx}]: missing length")))?
                as u64;
            let path_list = f
                .dict_get_list("path")
                .ok_or_else(|| Error::Metainfo(format!("files[{idx}]: missing path")))?;
            // Root directory = torrent name (same as rtorrent `d.directory` + name).
            let mut path = PathBuf::from(&torrent_name);
            for p in path_list {
                let s = p.as_str().ok_or_else(|| {
                    Error::Metainfo(format!("files[{idx}]: path component not str"))
                })?;
                let comp = normalize_path_component(s)
                    .map_err(|e| Error::Metainfo(format!("files[{idx}]: {e}")))?;
                path.push(comp);
            }
            out.push(TorrentFile {
                path,
                size: length,
                offset,
            });
            offset = offset.saturating_add(length);
        }
        return Ok(out);
    }

    // Single-file: path is just the (normalized) name.
    let length = info
        .dict_get_int("length")
        .ok_or_else(|| Error::Metainfo("missing length or files".into()))? as u64;
    Ok(vec![TorrentFile {
        path: PathBuf::from(torrent_name),
        size: length,
        offset: 0,
    }])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal single-file torrent (hand-built bencode).
    fn sample_torrent() -> Vec<u8> {
        // info: name=test, piece length=16384, length=1, pieces=20 bytes
        let mut pieces = vec![0u8; 20];
        pieces[0] = 0xaa;
        let mut info = Vec::new();
        info.extend_from_slice(b"d6:lengthi1e4:name4:test12:piece lengthi16384e6:pieces20:");
        info.extend_from_slice(&pieces);
        info.extend_from_slice(b"e");
        let mut root = Vec::new();
        // "http://x" is 8 bytes
        root.extend_from_slice(b"d8:announce8:http://x4:info");
        root.extend_from_slice(&info);
        root.extend_from_slice(b"e");
        root
    }

    #[test]
    fn parse_sample() {
        let data = sample_torrent();
        let m = Metainfo::parse_bytes(&data).unwrap();
        assert_eq!(m.name, "test");
        assert!(!m.is_multi_file);
        assert_eq!(m.piece_length, 16384);
        assert_eq!(m.piece_count, 1);
        assert_eq!(m.total_size, 1);
        assert_eq!(m.files.len(), 1);
        assert_eq!(m.files[0].path, PathBuf::from("test"));
        assert_eq!(m.trackers.len(), 1);
        assert_eq!(m.infohash_hex().len(), 40);
    }

    #[test]
    fn multi_file_paths_include_torrent_name() {
        // info: name=pack, files=[{path:[a,x],length:1},{path:[b],length:1}], piece length=16384
        let pieces = vec![0u8; 20];
        // d4:name4:pack5:filesl d6:lengthi1e4:pathl1:a1:xee d6:lengthi1e4:pathl1:bee e12:piece lengthi16384e6:pieces20:…e
        let mut info = Vec::new();
        info.extend_from_slice(b"d4:name4:pack5:filesl");
        info.extend_from_slice(b"d6:lengthi1e4:pathl1:a1:xee");
        info.extend_from_slice(b"d6:lengthi1e4:pathl1:bee");
        info.extend_from_slice(b"e12:piece lengthi16384e6:pieces20:");
        info.extend_from_slice(&pieces);
        info.extend_from_slice(b"e");
        let mut root = Vec::new();
        root.extend_from_slice(b"d8:announce8:http://x4:info");
        root.extend_from_slice(&info);
        root.extend_from_slice(b"e");

        let m = Metainfo::parse_bytes(&root).unwrap();
        assert!(m.is_multi_file);
        assert_eq!(m.name, "pack");
        assert_eq!(m.files.len(), 2);
        assert_eq!(m.files[0].path, PathBuf::from("pack/a/x"));
        assert_eq!(m.files[1].path, PathBuf::from("pack/b"));
    }

    #[test]
    fn normalize_path_component_blocks_traversal() {
        assert!(normalize_path_component("..").is_err());
        assert!(normalize_path_component(".").is_err());
        assert!(normalize_path_component("").is_err());
        // Separators / controls rewritten — no traversal left.
        assert_eq!(normalize_path_component("a/b").unwrap(), "a_b");
        assert_eq!(
            normalize_path_component("Cool.Show.S01").unwrap(),
            "Cool.Show.S01"
        );
        assert_eq!(normalize_path_component("foo:bar").unwrap(), "foo:bar"); // Unix-ok
        let n = normalize_path_component("\0evil").unwrap();
        assert!(!n.contains('\0'));
        assert_eq!(n, "evil");
    }
}
