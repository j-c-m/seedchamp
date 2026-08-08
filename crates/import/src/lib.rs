//! Session importers: rtorrent and Transmission.
//!
//! rtorrent layout:
//!   `<INFOHASH40>.torrent`
//!   `<INFOHASH40>.torrent.rtorrent`
//!   `<INFOHASH40>.torrent.libtorrent_resume`
//!
//! Transmission layout (config root):
//!   `torrents/<INFOHASH40>.torrent`
//!   `resume/<INFOHASH40>.resume`

#![forbid(unsafe_code)]

mod common;
mod resume;
mod rtorrent_side;
mod session;
mod transmission;

pub use common::{ImportOptions, ImportReport};
pub use session::{import_session, import_session_with};
pub use transmission::{import_transmission, import_transmission_with};
