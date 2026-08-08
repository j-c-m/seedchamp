//! BitTorrent wire protocol (BEP 3, BEP 6 Fast, BEP 10).

pub mod fast;
pub mod messages;
pub mod peer_id;

pub use fast::*;
pub use messages::*;
pub use peer_id::{identify_peer_id, ltep_client_version, prefer_client_label};
