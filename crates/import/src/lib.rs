//! rtorrent session import.
//!
//! Session layout (rtorrent / libtorrent session dir):
//!   `<INFOHASH40>.torrent`
//!   `<INFOHASH40>.torrent.rtorrent`
//!   `<INFOHASH40>.torrent.libtorrent_resume`

#![forbid(unsafe_code)]

mod resume;
mod rtorrent_side;
mod session;

pub use session::{import_session, import_session_with, ImportOptions, ImportReport};
