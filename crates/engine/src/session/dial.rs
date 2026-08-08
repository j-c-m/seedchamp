//! Outbound peer dial list: manual + peer_cache + tracker compact list.

use std::collections::HashSet;
use std::net::SocketAddr;

/// Max rows kept in SQLite `peer_cache` per torrent after prune.
pub(super) const PEER_CACHE_KEEP: usize = 200;
/// How many cached addrs to load when building a dial list.
pub(super) const PEER_CACHE_DIAL_LOAD: usize = 80;

/// Build outbound dial list: manual first, then shuffled cache + tracker (deduped).
///
/// Cache is preferred over brand-new tracker peers (often more useful after a
/// prior session). Each non-manual tier is shuffled so we do not always dial
/// the same first N addresses from the tracker compact list.
pub(super) fn merge_outbound_peers(
    manual: &[SocketAddr],
    tracker: Vec<SocketAddr>,
    cached: Vec<SocketAddr>,
    max: usize,
) -> Vec<SocketAddr> {
    use rand::seq::SliceRandom;
    let max = max.max(1);
    let mut seen = HashSet::with_capacity(max * 2);
    let mut out = Vec::with_capacity(max);

    for &a in manual {
        if seen.insert(a) {
            out.push(a);
            if out.len() >= max {
                return out;
            }
        }
    }

    let mut cache: Vec<SocketAddr> = cached.into_iter().filter(|a| !seen.contains(a)).collect();
    let mut track: Vec<SocketAddr> = tracker.into_iter().filter(|a| !seen.contains(a)).collect();
    let mut rng = rand::rng();
    cache.shuffle(&mut rng);
    track.shuffle(&mut rng);

    for a in cache.into_iter().chain(track) {
        if seen.insert(a) {
            out.push(a);
            if out.len() >= max {
                break;
            }
        }
    }
    out
}

#[cfg(test)]
mod peer_cache_dial_tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddr};

    fn v4(o: u8, port: u16) -> SocketAddr {
        SocketAddr::from((Ipv4Addr::new(10, 0, 0, o), port))
    }

    #[test]
    fn manual_first_then_dedupe() {
        let manual = vec![v4(1, 6881)];
        let tracker = vec![v4(1, 6881), v4(2, 6881)];
        let cached = vec![v4(2, 6881), v4(3, 6881)];
        let dial = merge_outbound_peers(&manual, tracker, cached, 10);
        assert_eq!(dial[0], v4(1, 6881));
        assert_eq!(dial.len(), 3);
        assert!(dial.contains(&v4(2, 6881)));
        assert!(dial.contains(&v4(3, 6881)));
    }

    #[test]
    fn respects_max() {
        let tracker: Vec<_> = (1..=20).map(|i| v4(i, 6881)).collect();
        let dial = merge_outbound_peers(&[], tracker, vec![], 5);
        assert_eq!(dial.len(), 5);
    }
}
