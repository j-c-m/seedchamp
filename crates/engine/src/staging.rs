//! Piece staging: assemble blocks in RAM. SHA-1 and disk write are HashPool / DiskWorker.

pub mod piece_pool;
pub mod pool;

pub use piece_pool::{
    buffer_count_for_limit, PieceBufferPool, DEFAULT_STAGING_MEM_LIMIT, MAX_PIECE_BUFFERS,
    MIN_PIECE_BUFFERS,
};
pub use pool::{block_len, num_blocks, StagingPool, BLOCK_SIZE};
