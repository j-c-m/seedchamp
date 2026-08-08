//! Disk I/O: spans, fd cache, positioned reads/writes.

pub mod audit;
pub mod fd_cache;
pub mod read;
pub mod relocate;
pub mod spans;
pub mod write;

pub use audit::{check_complete_layout, StorageFileProblem, StorageProblemKind};
pub use fd_cache::{open_read_compio_peer, with_peer_fd_cache, FdCache};
pub use read::{hash_piece_windowed, read_piece, read_span, read_span_blocking, HASH_READ_WINDOW};
pub use relocate::{
    expand_user_path, relocate_torrent_data, transfer_payload_files, TransferStats,
};
pub use spans::{FileLayout, IoSpan, StorageLayout};
pub use write::{ensure_storage, ensure_storage_with_priorities, write_piece, write_span};
