//! SQLite session catalog.

mod open;
mod peers;
mod pieces;
mod queries;
mod settings;
mod stats;
mod storage;
mod torrent;
mod trackers;
mod types;

pub use open::Catalog;
pub use queries::{decode_peer_addr, encode_peer_addr, InsertOutcome, StorageAuditReport};
pub use trackers::TrackerAnnounceUpdate;
pub use types::{
    all_set_bitfield, bitfield_get, bitfield_set, bitfield_size_bytes, count_have_bits,
    empty_bitfield, FileProgress, FileRow, SessionLimits, TorrentDetail, TorrentInsert,
    TorrentListRow, TrackerRow,
};
