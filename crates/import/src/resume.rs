//! Parse `.libtorrent_resume` bencode.

use seedchamp_engine::bencode::{self, Value};
use seedchamp_engine::catalog::{bitfield_size_bytes, count_have_bits};
use seedchamp_engine::{Error, Result};

#[derive(Debug, Default)]
pub struct ResumeData {
    pub bitfield: Option<Vec<u8>>,
    pub have_count: u32,
    pub complete: bool,
    pub uploaded: u64,
    pub downloaded: u64,
    pub file_priorities: Vec<i32>,
}

pub fn parse_resume(bytes: &[u8], piece_count: u32) -> Result<ResumeData> {
    let root = bencode::decode_full(bytes).map_err(|e| Error::Msg(format!("resume: {e}")))?;
    let mut out = ResumeData::default();

    // bitfield: string (bytes) or integer (all complete / empty)
    if let Some(v) = root.dict_get("bitfield") {
        match v {
            Value::Bytes(b) => {
                let expect = bitfield_size_bytes(piece_count);
                if b.len() == expect {
                    out.have_count = count_have_bits(b, piece_count);
                    out.complete = out.have_count == piece_count && piece_count > 0;
                    out.bitfield = if out.complete { None } else { Some(b.clone()) };
                }
            }
            Value::Int(n) => {
                if *n as u32 == piece_count && piece_count > 0 {
                    out.complete = true;
                    out.have_count = piece_count;
                    out.bitfield = None;
                } else if *n == 0 {
                    out.complete = false;
                    out.have_count = 0;
                    out.bitfield = Some(vec![0u8; bitfield_size_bytes(piece_count)]);
                }
            }
            _ => {}
        }
    }

    if let Some(n) = root.dict_get_int("uploaded") {
        out.uploaded = n.max(0) as u64;
    }
    if let Some(n) = root.dict_get_int("downloaded") {
        out.downloaded = n.max(0) as u64;
    }

    // files: list of dicts with priority
    if let Some(files) = root.dict_get_list("files") {
        for f in files {
            let prio = f.dict_get_int("priority").unwrap_or(1) as i32;
            out.file_priorities.push(prio);
        }
    }

    Ok(out)
}
