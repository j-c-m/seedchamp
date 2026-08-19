use super::*;
use crate::config::Config;

#[test]
fn maps_buffers_limits_and_tracker() {
    let mut c = Config::default();
    c.network.send_buffer_bytes = 4 * 1024 * 1024;
    c.network.recv_buffer_bytes = 2 * 1024 * 1024;
    c.limits.min_peers = 40;
    c.limits.max_peers = 8;
    c.limits.seed_dial_peers = true;
    c.limits.max_connections = 100;
    c.limits.redundant_seed_idle_secs = 0;
    c.limits.useless_peer_idle_secs = 90;
    c.tracker.max_concurrent_per_host = 3;
    c.tracker.startup_stagger_ms = 10;
    c.tracker.max_inflight_announces = 4;
    c.tracker.numwant = 80;
    let rt = RuntimeConfig::from_config(&c).unwrap();
    assert_eq!(rt.send_buffer_bytes, 4 * 1024 * 1024);
    assert_eq!(rt.recv_buffer_bytes, 2 * 1024 * 1024);
    assert_eq!(rt.max_peers, 8);
    assert_eq!(rt.min_peers, 8); // clamped ≤ max
    assert!(rt.seed_dial_peers);
    assert_eq!(rt.max_connections, 100);
    assert_eq!(rt.redundant_seed_idle_secs, 0);
    assert_eq!(rt.useless_peer_idle_secs, 90);
    assert_eq!(rt.max_concurrent_per_host, 3);
    assert_eq!(rt.startup_stagger_ms, 10);
    assert_eq!(rt.max_inflight_announces, 4);
    assert_eq!(rt.numwant, 80);
}

#[test]
fn clamps_zero_limits_to_one() {
    let mut c = Config::default();
    c.limits.min_peers = 0;
    c.limits.max_peers = 0;
    c.limits.max_connections = 0;
    let rt = RuntimeConfig::from_config(&c).unwrap();
    assert_eq!(rt.max_peers, 1);
    assert_eq!(rt.min_peers, 0); // min may be 0; max floor is 1
    assert_eq!(rt.max_connections, 1);
}

#[test]
fn peer_limit_defaults() {
    let rt = RuntimeConfig::from_config(&Config::default()).unwrap();
    assert_eq!(rt.min_peers, 20);
    assert_eq!(rt.max_peers, 40);
    assert!(!rt.seed_dial_peers);
}

#[test]
fn peer_id_prefix_and_user_agent() {
    let c = Config::default();
    let rt = RuntimeConfig::from_config(&c).unwrap();
    assert_eq!(rt.peer_id_prefix, b"-sc0001-");
    assert_eq!(rt.http_user_agent, crate::tracker::tracker_user_agent());
    assert_eq!(
        rt.ltep_client,
        format!("seedchamp {}", crate::library::pkg_version_major())
    );
    assert_eq!(rt.ltep_client, "seedchamp 1");
    assert_eq!(rt.http_user_agent, "seedchamp/1");

    // Explicit overrides for peer id prefix, UA, and LTEP.
    let mut c = Config::default();
    c.network.peer_id_prefix = "-sc9999-".into();
    c.network.http_user_agent = "seedchamp-test/9".into();
    c.network.ltep_client = "custom-client 9".into();
    let rt = RuntimeConfig::from_config(&c).unwrap();
    assert_eq!(rt.peer_id_prefix, b"-sc9999-");
    assert_eq!(rt.http_user_agent, "seedchamp-test/9");
    assert_eq!(rt.ltep_client, "custom-client 9");
}
