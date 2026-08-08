//! UDP tracker announce (BEP 15) — Compio on the tracker thread.
//!
//! Host resolve uses cyper-hickory (Compio UDP DNS), not blocking getaddrinfo.

use std::net::SocketAddr;
use std::time::Duration;

use compio::buf::BufResult;
use compio::net::UdpSocket;
use rand::Rng;

use super::dns;
use super::http::{AnnounceRequest, AnnounceResponse};
use crate::error::{Error, Result};

const PROTOCOL_ID: u64 = 0x0000_0417_2710_1980;
const ACTION_CONNECT: u32 = 0;
const ACTION_ANNOUNCE: u32 = 1;
const ACTION_ERROR: u32 = 3;

const UDP_IO_TIMEOUT: Duration = Duration::from_secs(5);

/// Parse `udp://host:port/announce` → host:port.
pub fn parse_udp_tracker_url(url: &str) -> Result<(String, u16)> {
    let u = url.trim();
    let rest = u
        .strip_prefix("udp://")
        .or_else(|| u.strip_prefix("UDP://"))
        .ok_or_else(|| Error::Msg(format!("not a udp tracker url: {url}")))?;
    let hostport = rest.split('/').next().unwrap_or(rest);
    let (host, port) = if let Some((h, p)) = hostport.rsplit_once(':') {
        let port: u16 = p
            .parse()
            .map_err(|_| Error::Msg(format!("bad udp port in {url}")))?;
        (h.to_string(), port)
    } else {
        (hostport.to_string(), 80)
    };
    if host.is_empty() {
        return Err(Error::Msg("empty udp host".into()));
    }
    Ok((host, port))
}

fn build_connect_packet(tid: u32) -> [u8; 16] {
    let mut pkt = [0u8; 16];
    pkt[0..8].copy_from_slice(&PROTOCOL_ID.to_be_bytes());
    pkt[8..12].copy_from_slice(&ACTION_CONNECT.to_be_bytes());
    pkt[12..16].copy_from_slice(&tid.to_be_bytes());
    pkt
}

fn build_announce_packet(conn_id: u64, tid: u32, req: &AnnounceRequest) -> Vec<u8> {
    let event: u32 = match req.event {
        Some("completed") => 1,
        Some("started") => 2,
        Some("stopped") => 3,
        _ => 0,
    };
    // Stable per-torrent key (rtorrent); fall back to random if unset.
    let key = if req.key != 0 {
        req.key
    } else {
        rand::rng().next_u32()
    };
    let numwant = req.numwant as i32;

    let mut pkt = Vec::with_capacity(98);
    pkt.extend_from_slice(&conn_id.to_be_bytes());
    pkt.extend_from_slice(&ACTION_ANNOUNCE.to_be_bytes());
    pkt.extend_from_slice(&tid.to_be_bytes());
    pkt.extend_from_slice(&req.infohash);
    pkt.extend_from_slice(&req.peer_id);
    pkt.extend_from_slice(&req.downloaded.to_be_bytes());
    pkt.extend_from_slice(&req.left.to_be_bytes());
    pkt.extend_from_slice(&req.uploaded.to_be_bytes());
    pkt.extend_from_slice(&event.to_be_bytes());
    pkt.extend_from_slice(&0u32.to_be_bytes()); // IP
    pkt.extend_from_slice(&key.to_be_bytes());
    pkt.extend_from_slice(&numwant.to_be_bytes());
    pkt.extend_from_slice(&req.port.to_be_bytes());
    debug_assert_eq!(pkt.len(), 98);
    pkt
}

fn parse_connect_response(buf: &[u8], n: usize, tid: u32) -> Result<u64> {
    if n < 16 {
        return Err(Error::Msg("udp connect short response".into()));
    }
    let action = u32::from_be_bytes(buf[0..4].try_into().unwrap());
    let rtid = u32::from_be_bytes(buf[4..8].try_into().unwrap());
    if rtid != tid {
        return Err(Error::Msg("udp connect transaction mismatch".into()));
    }
    if action == ACTION_ERROR {
        let msg = String::from_utf8_lossy(&buf[8..n]).to_string();
        return Err(Error::Msg(format!("udp connect error: {msg}")));
    }
    if action != ACTION_CONNECT {
        return Err(Error::Msg(format!("udp connect bad action {action}")));
    }
    Ok(u64::from_be_bytes(buf[8..16].try_into().unwrap()))
}

fn parse_announce_response_udp(buf: &[u8], n: usize, tid: u32) -> Result<AnnounceResponse> {
    if n < 20 {
        return Err(Error::Msg("udp announce short response".into()));
    }
    let action = u32::from_be_bytes(buf[0..4].try_into().unwrap());
    let rtid = u32::from_be_bytes(buf[4..8].try_into().unwrap());
    if rtid != tid {
        return Err(Error::Msg("udp announce transaction mismatch".into()));
    }
    if action == ACTION_ERROR {
        let msg = String::from_utf8_lossy(&buf[8..n]).to_string();
        return Ok(AnnounceResponse {
            failure: Some(msg),
            ..Default::default()
        });
    }
    if action != ACTION_ANNOUNCE {
        return Err(Error::Msg(format!("udp announce bad action {action}")));
    }

    let interval = u32::from_be_bytes(buf[8..12].try_into().unwrap());
    // BEP 15: leechers @12, seeders @16 (HTTP keys are complete/incomplete).
    let leechers = u32::from_be_bytes(buf[12..16].try_into().unwrap());
    let seeders = u32::from_be_bytes(buf[16..20].try_into().unwrap());
    // UDP compact announce has no min interval field; session uses defaults.
    let mut out = AnnounceResponse {
        interval,
        min_interval: 0,
        peers: Vec::new(),
        failure: None,
        complete: Some(seeders),
        incomplete: Some(leechers),
    };
    let peers = &buf[20..n];
    for chunk in peers.chunks_exact(6) {
        let ip = std::net::Ipv4Addr::new(chunk[0], chunk[1], chunk[2], chunk[3]);
        let port = u16::from_be_bytes([chunk[4], chunk[5]]);
        if port != 0 {
            out.peers.push(SocketAddr::from((ip, port)));
        }
    }
    Ok(out)
}

/// UDP announce on the Compio tracker runtime (hickory resolve + socket + timeouts).
pub async fn announce_udp(req: &AnnounceRequest) -> Result<AnnounceResponse> {
    let (host, port) = parse_udp_tracker_url(&req.announce_url)?;
    // Same DNS path as cyper HTTP (hickory over Compio UDP).
    let addrs = dns::resolve_ipv4(&host, port, 2).await?;

    let sock = UdpSocket::bind("0.0.0.0:0")
        .await
        .map_err(|e| Error::Msg(format!("udp bind: {e}")))?;

    let mut last_err = Error::Msg("udp announce failed".into());
    for addr in addrs {
        match announce_udp_addr(&sock, addr, req).await {
            Ok(r) => return Ok(r),
            Err(e) => last_err = e,
        }
    }
    Err(last_err)
}

async fn announce_udp_addr(
    sock: &UdpSocket,
    addr: SocketAddr,
    req: &AnnounceRequest,
) -> Result<AnnounceResponse> {
    let tid = rand::rng().next_u32();
    let pkt = build_connect_packet(tid).to_vec();
    let BufResult(send_r, _) = sock.send_to(pkt, addr).await;
    send_r.map_err(|e| Error::Msg(format!("udp connect send: {e}")))?;

    let buf = vec![0u8; 2048];
    let (n, buf) = match compio::time::timeout(UDP_IO_TIMEOUT, sock.recv_from(buf)).await {
        Ok(BufResult(Ok((n, _from)), buf)) => (n, buf),
        Ok(BufResult(Err(e), _)) => {
            return Err(Error::Msg(format!("udp connect recv: {e}")));
        }
        Err(_) => return Err(Error::Msg("udp connect recv timeout".into())),
    };
    let conn_id = parse_connect_response(&buf, n, tid)?;

    let tid = rand::rng().next_u32();
    let pkt = build_announce_packet(conn_id, tid, req);
    let BufResult(send_r, _) = sock.send_to(pkt, addr).await;
    send_r.map_err(|e| Error::Msg(format!("udp announce send: {e}")))?;

    let buf = vec![0u8; 4096];
    let (n, buf) = match compio::time::timeout(UDP_IO_TIMEOUT, sock.recv_from(buf)).await {
        Ok(BufResult(Ok((n, _from)), buf)) => (n, buf),
        Ok(BufResult(Err(e), _)) => {
            return Err(Error::Msg(format!("udp announce recv: {e}")));
        }
        Err(_) => return Err(Error::Msg("udp announce recv timeout".into())),
    };
    parse_announce_response_udp(&buf, n, tid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_url() {
        let (h, p) = parse_udp_tracker_url("udp://tracker.opentrackr.org:1337/announce").unwrap();
        assert_eq!(h, "tracker.opentrackr.org");
        assert_eq!(p, 1337);
    }

    #[test]
    fn connect_packet_roundtrip_parse() {
        let tid = 0xAABB_CCDD;
        let pkt = build_connect_packet(tid);
        // Fake a connect response: action=0, tid, conn_id
        let mut resp = [0u8; 16];
        resp[0..4].copy_from_slice(&ACTION_CONNECT.to_be_bytes());
        resp[4..8].copy_from_slice(&tid.to_be_bytes());
        resp[8..16].copy_from_slice(&0x1122_3344_5566_7788u64.to_be_bytes());
        let id = parse_connect_response(&resp, 16, tid).unwrap();
        assert_eq!(id, 0x1122_3344_5566_7788);
        let _ = pkt;
    }

    /// Compio UDP bind/send/recv of a BEP15 connect + canned response on loopback.
    #[test]
    fn loopback_connect_exchange() {
        let rt = compio::runtime::Runtime::new().expect("rt");
        rt.block_on(async {
            let server = UdpSocket::bind("127.0.0.1:0").await.expect("bind server");
            let server_addr = server.local_addr().expect("server addr");
            let client = UdpSocket::bind("127.0.0.1:0").await.expect("bind client");

            let tid = 0x1122_3344u32;
            let pkt = build_connect_packet(tid).to_vec();
            client.send_to(pkt, server_addr).await.0.expect("send");

            let ((n, from), buf) = server.recv_from(vec![0u8; 64]).await.unwrap();
            assert_eq!(n, 16);
            assert_eq!(&buf[..16], &build_connect_packet(tid));

            // Reply with connection id.
            let mut resp = [0u8; 16];
            resp[0..4].copy_from_slice(&ACTION_CONNECT.to_be_bytes());
            resp[4..8].copy_from_slice(&tid.to_be_bytes());
            resp[8..16].copy_from_slice(&0xAAu64.to_be_bytes());
            server.send_to(resp.to_vec(), from).await.0.expect("reply");

            let ((n, _), buf) = client.recv_from(vec![0u8; 64]).await.unwrap();
            let conn_id = parse_connect_response(&buf, n, tid).expect("parse");
            assert_eq!(conn_id, 0xAA);
        });
    }
}
