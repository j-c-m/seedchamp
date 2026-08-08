//! HTTP(S) tracker announce (BEP 3) — Compio via cyper.
//!
//! cyper's `Client` is thread-local (`Rc` inner; `!Send` / `!Sync`). All announce
//! HTTP runs on `seedchamp-trk`; metainfo fetch builds a client on its private RT.

use std::cell::RefCell;
use std::sync::OnceLock;
use std::time::Duration;

use crate::error::{Error, Result};

/// HTTP `User-Agent` for tracker announces — **major only** (`seedchamp/1`).
///
/// Override via `network.http_user_agent` / `SEEDCHAMP_HTTP_USER_AGENT`.
/// Full package/git version is for CLI/`doctor` only ([`crate::VERSION`]).
pub fn tracker_user_agent() -> &'static str {
    static UA: OnceLock<String> = OnceLock::new();
    UA.get_or_init(|| format!("seedchamp/{}", crate::library::pkg_version_major()))
        .as_str()
}

#[derive(Debug, Clone)]
pub struct AnnounceRequest {
    pub announce_url: String,
    pub infohash: [u8; 20],
    pub peer_id: [u8; 20],
    pub port: u16,
    pub uploaded: u64,
    pub downloaded: u64,
    pub left: u64,
    pub event: Option<&'static str>, // started|completed|stopped
    pub numwant: u32,
    /// HTTP User-Agent (default [`tracker_user_agent`]).
    pub user_agent: String,
    /// rtorrent-style announce key (`&key=` hex). `0` = omit (should not happen).
    pub key: u32,
}

#[derive(Debug, Default)]
pub struct AnnounceResponse {
    /// Recommended re-announce interval (seconds).
    pub interval: u32,
    /// Tracker `min interval` (seconds); 0 if omitted. Starved re-requests must
    /// not go faster than this (300s floor applied at session layer).
    pub min_interval: u32,
    pub peers: Vec<std::net::SocketAddr>,
    pub failure: Option<String>,
    /// Swarm seeders from announce (`complete` key / UDP seeders field).
    pub complete: Option<u32>,
    /// Swarm leechers from announce (`incomplete` key / UDP leechers field).
    pub incomplete: Option<u32>,
}

/// Resolve effective User-Agent (empty → [`tracker_user_agent`]).
pub fn effective_user_agent(req: &AnnounceRequest) -> &str {
    let ua = req.user_agent.trim();
    if ua.is_empty() {
        tracker_user_agent()
    } else {
        ua
    }
}

/// Build announce URL with query string (info_hash / peer_id URL-encoded as raw bytes).
pub fn build_announce_url(req: &AnnounceRequest) -> Result<String> {
    let base = req.announce_url.trim();
    if base.is_empty() {
        return Err(Error::Msg("empty announce url".into()));
    }
    let sep = if base.contains('?') { '&' } else { '?' };
    let mut url = format!(
        "{base}{sep}info_hash={}&peer_id={}&port={}&uploaded={}&downloaded={}&left={}&compact=1&numwant={}",
        percent_encode_bytes(&req.infohash),
        percent_encode_bytes(&req.peer_id),
        req.port,
        req.uploaded,
        req.downloaded,
        req.left,
        req.numwant,
    );
    if let Some(ev) = req.event {
        url.push_str("&event=");
        url.push_str(ev);
    }
    // rtorrent: only emit when non-zero (we always use non-zero keys).
    if req.key != 0 {
        url.push_str(&format!("&key={:08x}", req.key));
    }
    Ok(url)
}

fn percent_encode_bytes(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 3);
    for &x in b {
        s.push('%');
        s.push_str(&format!("{x:02X}"));
    }
    s
}

fn build_http_client() -> cyper::Client {
    // rustls needs a process CryptoProvider. Our Cargo.toml enables rustls/ring so
    // ClientConfig::builder (used by cyper) can install ring automatically.
    // Without that feature, Client::build panics.
    // `hickory-dns` feature: async Compio DNS (same stack as tracker UDP resolve).
    cyper::Client::builder()
        .use_rustls_default()
        .hickory_dns(true)
        .redirect(cyper::redirect::Policy::limited(5))
        .build()
        .expect("cyper client")
}

/// Thread-local cyper client (connection pool; rustls).
///
/// cyper `Client` is not `Send`/`Sync`; keep one pool per Compio worker that
/// performs HTTP (tracker thread, short-lived fetch RT). Callers set
/// `User-Agent` per request. Deadlines use [`compio::time::timeout`].
fn thread_http_client() -> cyper::Client {
    thread_local! {
        static CLIENT: RefCell<Option<cyper::Client>> = const { RefCell::new(None) };
    }
    CLIENT.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            *slot = Some(build_http_client());
        }
        slot.as_ref().expect("cyper client").clone()
    })
}

const TRACKER_HTTP_TIMEOUT: Duration = Duration::from_secs(12);

/// HTTP(S) GET announce on a Compio runtime (tracker thread).
///
/// Sends [`effective_user_agent`] as the `User-Agent` header (default
/// [`tracker_user_agent`]).
pub async fn announce_http(req: &AnnounceRequest) -> Result<AnnounceResponse> {
    let url = build_announce_url(req)?;
    let ua = effective_user_agent(req).to_string();
    let bytes = http_get_bytes(&url, &ua, TRACKER_HTTP_TIMEOUT).await?;
    parse_announce_response(&bytes)
}

/// GET `url` with `User-Agent` and total timeout; return body bytes.
///
/// Deadline is enforced with [`compio::time::timeout`] (cyper has no per-request
/// timeout builder). Must run on a Compio runtime.
pub(crate) async fn http_get_bytes(
    url: &str,
    user_agent: &str,
    timeout: Duration,
) -> Result<Vec<u8>> {
    let client = thread_http_client();
    let url = url.to_string();
    let user_agent = user_agent.to_string();
    let fut = async move {
        let resp = client
            .get(&url)
            .map_err(|e| Error::Msg(format!("HTTP build: {e}")))?
            .header("User-Agent", &user_agent)
            .map_err(|e| Error::Msg(format!("HTTP header: {e}")))?
            .send()
            .await
            .map_err(|e| Error::Msg(format!("HTTP: {e}")))?;
        let status = resp.status();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| Error::Msg(format!("HTTP read: {e}")))?;
        if !status.is_success() {
            return Err(Error::Msg(format!(
                "HTTP status {status}: {}",
                String::from_utf8_lossy(&bytes)
                    .chars()
                    .take(200)
                    .collect::<String>()
            )));
        }
        Ok(bytes.to_vec())
    };
    match compio::time::timeout(timeout, fut).await {
        Ok(r) => r,
        Err(_) => Err(Error::Msg(format!(
            "HTTP timeout after {}s",
            timeout.as_secs()
        ))),
    }
}

pub fn parse_announce_response(bytes: &[u8]) -> Result<AnnounceResponse> {
    use crate::bencode;
    let v = bencode::decode_full(bytes).map_err(|e| Error::Msg(format!("tracker bencode: {e}")))?;
    let mut out = AnnounceResponse::default();
    if let Some(f) = v.dict_get_str("failure reason") {
        out.failure = Some(f.to_string());
        return Ok(out);
    }
    let interval = v.dict_get_int("interval").unwrap_or(1800).max(0) as u32;
    let min_interval = v.dict_get_int("min interval").unwrap_or(0).max(0) as u32;
    out.interval = if interval == 0 { 1800 } else { interval };
    out.min_interval = min_interval;
    // BEP 3 optional swarm stats (same keys as scrape).
    if let Some(n) = v.dict_get_int("complete") {
        if n >= 0 {
            out.complete = Some(n as u32);
        }
    }
    if let Some(n) = v.dict_get_int("incomplete") {
        if n >= 0 {
            out.incomplete = Some(n as u32);
        }
    }
    if let Some(peers) = v.dict_get_bytes("peers") {
        for chunk in peers.chunks_exact(6) {
            let ip = std::net::Ipv4Addr::new(chunk[0], chunk[1], chunk[2], chunk[3]);
            let port = u16::from_be_bytes([chunk[4], chunk[5]]);
            out.peers.push(std::net::SocketAddr::from((ip, port)));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_encodes_binary() {
        let req = AnnounceRequest {
            announce_url: "http://tracker.example/announce".into(),
            infohash: [
                0x00, 0xff, 0x20, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x01,
            ],
            peer_id: *b"-sc0001-\0\0\0\0\0\0\0\0\0\0\0\0",
            port: 6881,
            uploaded: 0,
            downloaded: 0,
            left: 0,
            event: Some("started"),
            numwant: 50,
            user_agent: tracker_user_agent().into(),
            key: 0x00ab_cdef,
        };
        let u = build_announce_url(&req).unwrap();
        assert!(u.contains("info_hash=%00%FF%20"));
        assert!(u.contains("event=started"));
        assert!(u.contains("compact=1"));
        assert!(u.contains("key=00abcdef"), "{u}");
    }

    #[test]
    fn effective_ua_default_and_override() {
        let mut req = AnnounceRequest {
            announce_url: "http://x/".into(),
            infohash: [0u8; 20],
            peer_id: [0u8; 20],
            port: 1,
            uploaded: 0,
            downloaded: 0,
            left: 0,
            event: None,
            numwant: 50,
            user_agent: String::new(),
            key: 0,
        };
        assert_eq!(effective_user_agent(&req), "seedchamp/1");
        assert_eq!(effective_user_agent(&req), tracker_user_agent());
        req.user_agent = "  ".into();
        assert_eq!(effective_user_agent(&req), tracker_user_agent());
        req.user_agent = "seedchamp/9".into();
        assert_eq!(effective_user_agent(&req), "seedchamp/9");
    }

    #[test]
    fn default_ua_is_major_only() {
        assert_eq!(tracker_user_agent(), "seedchamp/1");
        assert!(!tracker_user_agent().contains('.'));
    }

    #[test]
    fn parse_compact_peers() {
        // interval=1800, peers = 1.2.3.4:6881
        let mut body = b"d8:intervali1800e5:peers6:".to_vec();
        body.extend_from_slice(&[1, 2, 3, 4, 0x1a, 0xe1]);
        body.push(b'e');
        let r = parse_announce_response(&body).unwrap();
        assert_eq!(r.interval, 1800);
        assert_eq!(r.peers.len(), 1);
        assert_eq!(r.peers[0].to_string(), "1.2.3.4:6881");
        assert!(r.complete.is_none());
        assert!(r.incomplete.is_none());
    }

    #[test]
    fn parse_complete_incomplete() {
        // interval + complete + incomplete, no peers
        let body = b"d8:intervali900e8:completei42e10:incompletei7ee";
        let r = parse_announce_response(body).unwrap();
        assert_eq!(r.interval, 900);
        assert_eq!(r.complete, Some(42));
        assert_eq!(r.incomplete, Some(7));
        assert!(r.peers.is_empty());
    }

    /// Regression: cyper + rustls without a CryptoProvider panics on Client::build.
    #[test]
    fn https_client_builds() {
        let rt = compio::runtime::Runtime::new().expect("rt");
        rt.block_on(async {
            let _ = thread_http_client();
        });
    }
}
