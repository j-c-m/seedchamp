//! Dial eligibility: tracker IP:listen-port identity, cooldown backoff.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

/// Per-address dial failure / cooldown state (keyed by tracker listen endpoint).
#[derive(Clone, Debug)]
pub(super) struct DialData {
    pub(super) next_ok: Instant,
    pub(super) fails: u32,
}

/// Exponential backoff after dial/handshake failures (cap 30 min).
pub(super) fn backoff_delay(fails: u32) -> Duration {
    let exp = fails.saturating_sub(1).min(6);
    let secs = 30u64.saturating_mul(1u64 << exp).min(30 * 60);
    Duration::from_secs(secs.max(30))
}

pub(super) fn record_dial_fail(
    map: &mut HashMap<(i64, SocketAddr), DialData>,
    torrent_id: i64,
    addr: SocketAddr,
    now: Instant,
) {
    let e = map.entry((torrent_id, addr)).or_insert(DialData {
        next_ok: now,
        fails: 0,
    });
    e.fails = e.fails.saturating_add(1);
    e.next_ok = now + backoff_delay(e.fails);
}

pub(super) fn record_dial_soft_fail(
    map: &mut HashMap<(i64, SocketAddr), DialData>,
    torrent_id: i64,
    addr: SocketAddr,
    now: Instant,
) {
    record_dial_fail(map, torrent_id, addr, now);
}

pub(super) fn clear_dial_fail(
    map: &mut HashMap<(i64, SocketAddr), DialData>,
    torrent_id: i64,
    addr: SocketAddr,
) {
    map.remove(&(torrent_id, addr));
}

pub(super) fn light_disconnect_cooldown(
    map: &mut HashMap<(i64, SocketAddr), DialData>,
    torrent_id: i64,
    addr: SocketAddr,
    now: Instant,
) {
    let e = map.entry((torrent_id, addr)).or_insert(DialData {
        next_ok: now,
        fails: 0,
    });
    let quiet = now + Duration::from_secs(30);
    if e.next_ok < quiet {
        e.next_ok = quiet;
    }
}

pub(super) fn is_cooled_down(
    map: &HashMap<(i64, SocketAddr), DialData>,
    torrent_id: i64,
    addr: SocketAddr,
    now: Instant,
) -> bool {
    map.get(&(torrent_id, addr))
        .map(|d| now < d.next_ok)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddr};

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from((Ipv4Addr::new(1, 2, 3, 4), port))
    }

    #[test]
    fn backoff_grows_and_caps() {
        assert_eq!(backoff_delay(1).as_secs(), 30);
        assert_eq!(backoff_delay(2).as_secs(), 60);
        assert_eq!(backoff_delay(3).as_secs(), 120);
        assert_eq!(backoff_delay(20).as_secs(), 30 * 60);
    }

    #[test]
    fn fail_blocks_until_next_ok() {
        let mut map = HashMap::new();
        let now = Instant::now();
        let a = addr(6881);
        record_dial_fail(&mut map, 1, a, now);
        assert!(is_cooled_down(&map, 1, a, now));
        assert!(!is_cooled_down(&map, 1, a, now + Duration::from_secs(31)));
    }
}
