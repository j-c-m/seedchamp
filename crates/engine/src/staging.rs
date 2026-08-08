//! Piece staging: assemble blocks in RAM, SHA-1 verify, then disk write.

pub mod piece_pool;
pub mod pool;

pub use piece_pool::{
    buffer_count_for_limit, PieceBufferPool, DEFAULT_STAGING_MEM_LIMIT, MAX_PIECE_BUFFERS,
    MIN_PIECE_BUFFERS,
};
pub use pool::{
    block_len, commit_verified_piece, num_blocks, PendingPiece, StagingPool, BLOCK_SIZE,
};
