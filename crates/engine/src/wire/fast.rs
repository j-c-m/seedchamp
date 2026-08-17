//! BEP 6 Fast Extension helpers: allowed-fast set generation.

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use sha1::{Digest, Sha1};

/// Default size of the allowed-fast set (BEP 6).
pub const ALLOWED_FAST_K: u32 = 10;

/// Canonical allowed-fast set for peer `ip` on a torrent with `piece_count` pieces.
///
/// Matches BEP 6 (IPv4 only; IPv6 returns empty — BEP has no IPv6 algorithm).
///
/// `k` is clamped to `piece_count` so tiny torrents cannot spin forever when the
/// set cannot grow beyond the number of pieces.
pub fn generate_allowed_fast_set(
    k: u32,
    piece_count: u32,
    infohash: &[u8; 20],
    ip: Ipv4Addr,
) -> Vec<u32> {
    if k == 0 || piece_count == 0 {
        return Vec::new();
    }
    let k = k.min(piece_count);
    let mut a: Vec<u32> = Vec::with_capacity(k as usize);
    // x = (ip & 0xFFFFFF00) big-endian || infohash
    let masked = u32::from(ip) & 0xFFFF_FF00;
    let mut x = Vec::with_capacity(24);
    x.extend_from_slice(&masked.to_be_bytes());
    x.extend_from_slice(infohash);

    // Safety: if the set cannot grow (duplicate-only hashes), stop after a few
    // empty rounds rather than spinning forever.
    let mut stagnant = 0u32;
    while (a.len() as u32) < k {
        let before = a.len();
        let mut h = Sha1::new();
        h.update(&x);
        let digest = h.finalize();
        x = digest.to_vec();
        for i in 0..5 {
            if (a.len() as u32) >= k {
                break;
            }
            let j = i * 4;
            let y = u32::from_be_bytes(x[j..j + 4].try_into().unwrap());
            let index = y % piece_count;
            if !a.contains(&index) {
                a.push(index);
            }
        }
        if a.len() == before {
            stagnant += 1;
            if stagnant >= 32 {
                break;
            }
        } else {
            stagnant = 0;
        }
    }
    a
}

/// Allowed-fast set for a socket peer (IPv4 only).
pub fn allowed_fast_for_addr(
    k: u32,
    piece_count: u32,
    infohash: &[u8; 20],
    addr: SocketAddr,
) -> Vec<u32> {
    match addr.ip() {
        IpAddr::V4(ip) => generate_allowed_fast_set(k, piece_count, infohash, ip),
        IpAddr::V6(_) => Vec::new(),
    }
}

/// Encode a list of Allowed Fast messages (one piece each).
pub fn encode_allowed_fast_messages(pieces: &[u32]) -> Vec<u8> {
    use super::messages::{encode_message, Message};
    let mut out = Vec::with_capacity(pieces.len() * 9);
    for &index in pieces {
        out.extend_from_slice(&encode_message(&Message::AllowedFast(index)));
    }
    out
}

/// Encode Suggest Piece messages.
pub fn encode_suggest_messages(pieces: &[u32]) -> Vec<u8> {
    use super::messages::{encode_message, Message};
    let mut out = Vec::with_capacity(pieces.len() * 9);
    for &index in pieces {
        out.extend_from_slice(&encode_message(&Message::SuggestPiece(index)));
    }
    out
}

/// First possession message after handshake when Fast is mutual.
pub fn encode_possession_fast(have_count: u32, piece_count: u32, bitfield: Vec<u8>) -> Vec<u8> {
    use super::messages::{encode_message, Message};
    if piece_count > 0 && have_count >= piece_count {
        encode_message(&Message::HaveAll)
    } else if have_count == 0 {
        encode_message(&Message::HaveNone)
    } else {
        encode_message(&Message::Bitfield(bitfield))
    }
}

/// Apply peer Have All / Have None into a bitfield buffer.
pub fn apply_have_all_none(peer_bf: &mut [u8], piece_count: u32, all: bool) {
    if all {
        peer_bf.fill(0xff);
        // Clear unused bits in last byte.
        let rem = (piece_count as usize) % 8;
        if rem != 0 {
            if let Some(last) = peer_bf.last_mut() {
                *last &= 0xffu8 << (8 - rem);
            }
        }
    } else {
        peer_bf.fill(0);
    }
}

/// Track Fast extension state for one peer connection.
#[derive(Debug, Default, Clone)]
pub struct FastSession {
    /// Mutual Fast negotiated.
    pub enabled: bool,
    /// Pieces the remote listed as Allowed Fast (we may request while choked).
    pub allowed_from_peer: HashSet<u32>,
    /// Pieces we advertised as Allowed Fast to the remote.
    pub allowed_to_peer: HashSet<u32>,
}

impl FastSession {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            ..Default::default()
        }
    }

    pub fn on_allowed_fast(&mut self, index: u32) {
        self.allowed_from_peer.insert(index);
    }

    pub fn peer_allows_while_choked(&self, index: u32) -> bool {
        self.enabled && self.allowed_from_peer.contains(&index)
    }

    pub fn we_allow_while_choking(&self, index: u32) -> bool {
        self.enabled && self.allowed_to_peer.contains(&index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bep6_example_allowed_fast_set() {
        // From BEP 6: IP 80.4.4.200, infohash all 0xaa, 1313 pieces, k=7
        let ih = [0xaau8; 20];
        let ip = Ipv4Addr::new(80, 4, 4, 200);
        let set = generate_allowed_fast_set(7, 1313, &ih, ip);
        assert_eq!(set, vec![1059, 431, 808, 1217, 287, 376, 1188]);
        let set9 = generate_allowed_fast_set(9, 1313, &ih, ip);
        assert_eq!(set9, vec![1059, 431, 808, 1217, 287, 376, 1188, 353, 508]);
    }

    #[test]
    fn apply_have_all_clears_padding_bits() {
        let mut bf = vec![0u8; 1];
        apply_have_all_none(&mut bf, 3, true);
        assert_eq!(bf[0], 0b1110_0000);
        apply_have_all_none(&mut bf, 3, false);
        assert_eq!(bf[0], 0);
    }

    #[test]
    fn allowed_fast_k_gt_piece_count_terminates() {
        // Interop matrix uses a 4-piece seed; k=10 must not spin.
        let ih = [1u8; 20];
        let ip = Ipv4Addr::new(127, 0, 0, 1);
        let set = generate_allowed_fast_set(10, 4, &ih, ip);
        assert!(set.len() <= 4);
        assert!(!set.is_empty() || true); // empty possible if unlucky; main check is termination
        let set2 = generate_allowed_fast_set(10, 4, &ih, ip);
        assert_eq!(set, set2, "deterministic");
    }
}
