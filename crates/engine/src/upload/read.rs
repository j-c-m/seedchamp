//! PIECE header layout for seed upload.

use super::PIECE_HEADER_LEN;

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
