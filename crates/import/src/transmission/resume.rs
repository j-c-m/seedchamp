//! Parse Transmission `.resume` bencode (libtransmission resume file).
//!
//! Keys from Transmission 3/4 `resume.cc` / quarks (best-effort aliases).
//! Real daemons often use kebab-case (`added-date`, `done-date`).

use seedchamp_engine::bencode::{self, Value};
use seedchamp_engine::catalog::{bitfield_size_bytes, count_have_bits};
use seedchamp_engine::{Error, Result};

#[derive(Debug, Default)]
pub struct TransmissionResume {
    pub data_root: Option<String>,
    pub bitfield: Option<Vec<u8>>,
    pub have_count: u32,
    pub complete: bool,
    pub uploaded: u64,
    pub downloaded: u64,
    /// Per-file priority (0 = off / DND, ≥1 = on). Empty if unknown.
    pub file_priorities: Vec<i32>,
    pub created_at: Option<i64>,
    pub finished_at: Option<i64>,
}

pub fn parse_transmission_resume(bytes: &[u8], piece_count: u32) -> Result<TransmissionResume> {
    let root = bencode::decode_full(bytes).map_err(|e| Error::Msg(format!("tr resume: {e}")))?;
    let mut out = TransmissionResume::default();

    // download directory
    for key in ["destination", "download-dir", "download_dir"] {
        if let Some(s) = root.dict_get_str(key) {
            if !s.is_empty() {
                out.data_root = Some(s.to_string());
                break;
            }
        }
    }

    if let Some(n) = root.dict_get_int("uploaded") {
        out.uploaded = n.max(0) as u64;
    }
    if let Some(n) = root.dict_get_int("downloaded") {
        out.downloaded = n.max(0) as u64;
    }

    for key in ["added_date", "added-date", "addedDate"] {
        if let Some(n) = root.dict_get_int(key) {
            if n > 0 {
                out.created_at = Some(n);
                break;
            }
        }
    }
    for key in ["done_date", "done-date", "doneDate"] {
        if let Some(n) = root.dict_get_int(key) {
            if n > 0 {
                out.finished_at = Some(n);
                break;
            }
        }
    }

    // Completion is progress.blocks ("all" / "none" / raw *block* bitfield).
    // progress.pieces is the *checked* piece map — not have-complete.
    if let Some(prog) = root.dict_get("progress") {
        if let Some(blocks) = prog.dict_get("blocks") {
            apply_blocks_progress(&mut out, blocks, piece_count);
        } else if let Some(bf) = prog.dict_get("bitfield") {
            apply_blocks_progress(&mut out, bf, piece_count);
        }
    }

    // File wanted: prefer `dnd` (1 = do-not-download). Transmission `priority`
    // is -1/0/1 (low/normal/high), not off/on — do not treat 0 as off.
    if let Some(dnd) = root.dict_get_list("dnd") {
        for d in dnd {
            let off = match d {
                Value::Int(0) => false,
                Value::Int(_) => true,
                _ => false,
            };
            out.file_priorities.push(if off { 0 } else { 1 });
        }
    } else if let Some(prios) = root.dict_get_list("priority") {
        for p in prios {
            let _ = p.as_int().unwrap_or(0);
            out.file_priorities.push(1);
        }
    }

    Ok(out)
}

fn apply_blocks_progress(out: &mut TransmissionResume, blocks: &Value, piece_count: u32) {
    match blocks {
        Value::Bytes(b) if b == b"all" => {
            out.complete = piece_count > 0;
            out.have_count = piece_count;
            out.bitfield = None;
        }
        Value::Bytes(b) if b == b"none" || b.is_empty() => {
            out.complete = false;
            out.have_count = 0;
            out.bitfield = Some(vec![0u8; bitfield_size_bytes(piece_count)]);
        }
        Value::Bytes(b) => {
            // Block-level bitmap — cannot losslessly map to piece have bitfield.
            out.complete = false;
            let expect = bitfield_size_bytes(piece_count);
            if b.len() == expect {
                // Same size as piece bitfield (rare); treat as pieces.
                out.have_count = count_have_bits(b, piece_count);
                out.complete = out.have_count == piece_count && piece_count > 0;
                out.bitfield = if out.complete { None } else { Some(b.clone()) };
            } else {
                out.have_count = 0;
                out.bitfield = None;
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_all_complete_resume() {
        // blocks=all is completion; pieces=all alone is only "checked"
        let benc = b"d11:destination3:/dl10:added-datei1700000000e9:done-datei1700000500e10:downloadedi50e8:uploadedi200e6:pausedi1e8:progressd6:blocks3:all6:pieces3:allee";
        let r = parse_transmission_resume(benc, 10).unwrap();
        assert_eq!(r.data_root.as_deref(), Some("/dl"));
        assert!(r.complete);
        assert_eq!(r.have_count, 10);
        assert!(r.bitfield.is_none());
        assert_eq!(r.uploaded, 200);
        assert_eq!(r.downloaded, 50);
        assert_eq!(r.created_at, Some(1_700_000_000));
        assert_eq!(r.finished_at, Some(1_700_000_500));
    }

    #[test]
    fn pieces_all_is_not_complete() {
        let benc = b"d8:progressd6:pieces3:allee";
        let r = parse_transmission_resume(benc, 10).unwrap();
        assert!(!r.complete);
        assert_eq!(r.have_count, 0);
    }

    #[test]
    fn parse_dnd_priorities() {
        let benc = b"d3:dndli0ei1ei0eee";
        let r = parse_transmission_resume(benc, 3).unwrap();
        assert_eq!(r.file_priorities, vec![1, 0, 1]);
    }
}
