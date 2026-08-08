//! Tracker clients (HTTP + UDP) and per-host announce limiting.
//!
//! HTTP and UDP announces run on the Compio **tracker** thread (`seedchamp-trk`):
//! HTTP via cyper (+ hickory DNS), UDP via Compio `UdpSocket` + cyper-hickory DNS.
//! HTTP(S) User-Agent defaults to [`tracker_user_agent`] (`seedchamp/<major>`).

pub mod dns;
pub mod http;
pub mod limit;
pub mod udp;

pub use http::{
    announce_http, build_announce_url, effective_user_agent, parse_announce_response,
    tracker_user_agent, AnnounceRequest, AnnounceResponse,
};
pub use limit::{tracker_host_key, HostLimiter, HostPermit};
pub use udp::{announce_udp, parse_udp_tracker_url};

use std::sync::Arc;

/// rtorrent-style announce key: random `u32` in `1..=u32::MAX` (never 0).
pub fn generate_tracker_key() -> u32 {
    use rand::RngExt;
    let mut rng = rand::rng();
    loop {
        let k = rng.random::<u32>();
        if k != 0 {
            return k;
        }
    }
}

/// Announce over HTTP(S) or UDP depending on URL scheme.
///
/// Call from the Compio tracker runtime (both schemes are pure Compio).
pub async fn announce(req: &AnnounceRequest) -> crate::error::Result<AnnounceResponse> {
    let u = req.announce_url.trim().to_ascii_lowercase();
    if u.starts_with("udp://") {
        announce_udp(req).await
    } else if u.starts_with("http://") || u.starts_with("https://") {
        announce_http(req).await
    } else {
        Err(crate::error::Error::Msg(format!(
            "unsupported tracker scheme: {}",
            req.announce_url
        )))
    }
}

/// Announce with a per-host concurrency slot (Compio-friendly flume gate).
pub async fn announce_limited(
    req: &AnnounceRequest,
    limiter: &Arc<HostLimiter>,
) -> crate::error::Result<AnnounceResponse> {
    let key = tracker_host_key(&req.announce_url);
    let _permit = limiter.acquire(&key).await;
    announce(req).await
}
