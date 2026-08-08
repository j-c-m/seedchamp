//! Inbound / outbound connection establish (plain BT + MSE/PE).

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use compio::net::TcpStream;
use compio::time::timeout;
use rand::Rng;

use crate::crypto::{
    deobfuscate_req2, hash_req1, receiver_build_response, receiver_parse_initiator, select_crypto,
    DhKeyPair, EncryptionMode, MseSession, DH_PUB_LEN,
};
use crate::error::{Error, Result};
use crate::wire::{
    encode_handshake, identify_peer_id, looks_like_bt_handshake, parse_handshake, HANDSHAKE_LEN,
};

use super::super::net;
use super::config::PeerConfig;
use super::established::EstablishedPeer;
use super::helpers::{mse_to_wire, WireCrypto};
use crate::hot::{HotRegistry, HotTorrent};
use crate::session::{set_peer_crypto, PeerCrypto};

/// End-to-end budget for inbound establish (first byte → our HS reply / bind).
/// Without this, a peer that stops after the first byte can pin a LivePeer with
/// `torrent_id=0` forever (no timeout on later `read_exact`s).
const INBOUND_HANDSHAKE_BUDGET: Duration = Duration::from_secs(30);
/// Per-read ceiling during inbound handshake / MSE.
const HS_IO: Duration = Duration::from_secs(15);

/// Connect outbound and run a full-duplex peer session.
pub async fn run_outbound_peer(
    addr: std::net::SocketAddr,
    torrent: Arc<HotTorrent>,
    cfg: PeerConfig,
) -> Result<()> {
    let our_hs = encode_handshake(&torrent.infohash, &cfg.peer_id);

    // PreferPlain: plain first, PE retry on failure.
    // PreferRc4: PE first, plain retry on failure.
    // RequireRc4: PE only. Off: plain only.
    match cfg.encryption {
        EncryptionMode::PreferPlain => match dial_plain(addr, &torrent, &cfg, &our_hs).await {
            Ok((stream, peer_hs)) => {
                set_peer_crypto(&cfg.crypto, PeerCrypto::Plain);
                run_outbound_after_hs(addr, stream, torrent, cfg, None, peer_hs).await
            }
            Err(plain_err) => {
                tracing::trace!(
                    %addr,
                    torrent_id = torrent.id,
                    error = %plain_err,
                    "outbound prefer-plain: plain failed; retry PE"
                );
                match dial_pe(addr, &torrent, &cfg, EncryptionMode::PreferPlain, &our_hs).await {
                    Ok((stream, wire, peer_hs)) => {
                        run_outbound_after_hs(addr, stream, torrent, cfg, wire, peer_hs).await
                    }
                    Err(pe_err) => {
                        tracing::trace!(
                            %addr,
                            torrent_id = torrent.id,
                            plain_error = %plain_err,
                            pe_error = %pe_err,
                            "outbound prefer-plain: PE retry failed"
                        );
                        Err(Error::Msg(format!(
                            "prefer-plain: plain failed ({plain_err}); PE retry: {pe_err}"
                        )))
                    }
                }
            }
        },
        EncryptionMode::PreferRc4 => {
            match dial_pe(addr, &torrent, &cfg, EncryptionMode::PreferRc4, &our_hs).await {
                Ok((stream, wire, peer_hs)) => {
                    run_outbound_after_hs(addr, stream, torrent, cfg, wire, peer_hs).await
                }
                Err(pe_err) => {
                    tracing::trace!(
                        %addr,
                        torrent_id = torrent.id,
                        error = %pe_err,
                        "outbound prefer-rc4: PE failed; retry plain"
                    );
                    match dial_plain(addr, &torrent, &cfg, &our_hs).await {
                        Ok((stream, peer_hs)) => {
                            set_peer_crypto(&cfg.crypto, PeerCrypto::Plain);
                            run_outbound_after_hs(addr, stream, torrent, cfg, None, peer_hs).await
                        }
                        Err(plain_err) => {
                            tracing::trace!(
                                %addr,
                                torrent_id = torrent.id,
                                pe_error = %pe_err,
                                plain_error = %plain_err,
                                "outbound prefer-rc4: plain retry failed"
                            );
                            Err(Error::Msg(format!(
                                "prefer-rc4: PE failed ({pe_err}); plain retry: {plain_err}"
                            )))
                        }
                    }
                }
            }
        }
        EncryptionMode::RequireRc4 => {
            match dial_pe(addr, &torrent, &cfg, EncryptionMode::RequireRc4, &our_hs).await {
                Ok((stream, wire, peer_hs)) => {
                    run_outbound_after_hs(addr, stream, torrent, cfg, wire, peer_hs).await
                }
                Err(e) => {
                    tracing::trace!(
                        %addr,
                        torrent_id = torrent.id,
                        error = %e,
                        "outbound require-rc4: PE failed (no plain fallback)"
                    );
                    Err(e)
                }
            }
        }
        EncryptionMode::Off => match dial_plain(addr, &torrent, &cfg, &our_hs).await {
            Ok((stream, peer_hs)) => {
                set_peer_crypto(&cfg.crypto, PeerCrypto::Plain);
                run_outbound_after_hs(addr, stream, torrent, cfg, None, peer_hs).await
            }
            Err(e) => {
                // Often first-byte ≠ 19: remote insists on MSE/PE and we refuse.
                tracing::trace!(
                    %addr,
                    torrent_id = torrent.id,
                    error = %e,
                    "outbound encryption=off: plain failed (no PE fallback)"
                );
                Err(e)
            }
        },
    }
}

async fn run_outbound_after_hs(
    addr: std::net::SocketAddr,
    stream: TcpStream,
    torrent: Arc<HotTorrent>,
    cfg: PeerConfig,
    wire: Option<WireCrypto>,
    peer_hs: [u8; HANDSHAKE_LEN],
) -> Result<()> {
    // Self-check: remote peer id equals our local id.
    reject_if_self(&cfg.peer_id, &peer_hs)?;
    log_handshake_ok("outbound", &addr.to_string(), &torrent, &peer_hs, &cfg);
    EstablishedPeer::new(stream, torrent, cfg, wire, &peer_hs, &[])
        .run()
        .await
}

/// Drop connections where the remote handshake peer_id equals ours.
///
/// Without this, tracker/cache can return our own listen address; we dial ourselves,
/// accept the inbound half, and run two full-duplex peer loops on the **same**
/// process/hot torrent (self-swarm). That can pin peer slots and thrash the I/O path.
fn reject_if_self(our_peer_id: &[u8; 20], peer_hs: &[u8; HANDSHAKE_LEN]) -> Result<()> {
    let (_, their_id) = parse_handshake(peer_hs)?;
    if &their_id == our_peer_id {
        return Err(Error::Msg("is self".into()));
    }
    Ok(())
}

/// Accept / serve one inbound peer on the async worker pool.
pub async fn run_inbound_peer(
    stream: TcpStream,
    registry: Arc<parking_lot::RwLock<HotRegistry>>,
    cfg: PeerConfig,
) -> Result<()> {
    let _ = stream.set_nodelay(true);
    // Handshake only — the long-lived wire loop is not under this budget.
    let ready = match timeout(
        INBOUND_HANDSHAKE_BUDGET,
        establish_inbound(stream, registry, cfg),
    )
    .await
    {
        Ok(r) => r?,
        Err(_) => return Err(Error::Msg("inbound handshake timeout".into())),
    };
    let addr = ready
        .stream
        .peer_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| "?".into());
    log_handshake_ok("inbound", &addr, &ready.torrent, &ready.peer_hs, &ready.cfg);
    ready.run().await
}

/// Debug line after successful handshake (TUI log when capture ≥ debug).
fn log_handshake_ok(
    dir: &str,
    addr: &str,
    torrent: &HotTorrent,
    peer_hs: &[u8; HANDSHAKE_LEN],
    cfg: &PeerConfig,
) {
    let client = match parse_handshake(peer_hs) {
        Ok((_, pid)) => identify_peer_id(&pid),
        Err(_) => "unknown".into(),
    };
    let crypto = cfg
        .crypto
        .as_ref()
        .map(|a| PeerCrypto::from_u8(a.load(Ordering::Relaxed)).as_str())
        .unwrap_or("—");
    // Seed shared client label early (EstablishedPeer does this too).
    if let Some(slot) = cfg.client_label.as_ref() {
        *slot.lock() = client.clone();
    }
    tracing::debug!(
        dir,
        %addr,
        torrent_id = torrent.id,
        torrent = %torrent.name,
        client = %client,
        crypto,
        "peer handshake ok"
    );
}

/// Bound inbound peer ready for the established duplex loop.
struct InboundReady {
    stream: TcpStream,
    torrent: Arc<HotTorrent>,
    cfg: PeerConfig,
    wire: Option<WireCrypto>,
    peer_hs: [u8; HANDSHAKE_LEN],
    initial_plain: Vec<u8>,
}

impl InboundReady {
    async fn run(self) -> Result<()> {
        EstablishedPeer::new(
            self.stream,
            self.torrent,
            self.cfg,
            self.wire,
            &self.peer_hs,
            &self.initial_plain,
        )
        .run()
        .await
    }
}

/// First byte through torrent bind + our handshake reply (not the wire loop).
async fn establish_inbound(
    mut stream: TcpStream,
    registry: Arc<parking_lot::RwLock<HotRegistry>>,
    cfg: PeerConfig,
) -> Result<InboundReady> {
    let mut first = [0u8; 1];
    net::read_exact_timeout(&mut stream, &mut first, HS_IO).await?;

    if first[0] == 19 {
        if cfg.encryption.requires_rc4() {
            return Err(Error::Msg(
                "plain BT rejected: encryption mode requires RC4".into(),
            ));
        }
        let mut hs_buf = [0u8; HANDSHAKE_LEN];
        hs_buf[0] = first[0];
        net::read_exact_timeout(&mut stream, &mut hs_buf[1..], HS_IO).await?;
        return inbound_after_hs(stream, &hs_buf, registry, cfg, None, PeerCrypto::Plain).await;
    }

    if !cfg.encryption.wants_pe() {
        return Err(Error::Msg("peer tried PE but encryption is off".into()));
    }

    let (torrent, mse, ia) =
        pe_accept_incoming(&mut stream, first[0], &registry, cfg.encryption).await?;
    let crypto = if mse.rc4 {
        PeerCrypto::Rc4
    } else {
        PeerCrypto::PePlain
    };
    let wire = if mse.rc4 {
        Some(WireCrypto {
            encrypt: mse.encrypt,
            decrypt: mse.decrypt,
        })
    } else {
        None
    };

    if ia.len() >= HANDSHAKE_LEN && looks_like_bt_handshake(&ia[..HANDSHAKE_LEN]) {
        let (ih, _) = parse_handshake(&ia[..HANDSHAKE_LEN])?;
        if ih != torrent.infohash {
            return Err(Error::Msg("IA infohash mismatch".into()));
        }
        let mut peer_hs = [0u8; HANDSHAKE_LEN];
        peer_hs.copy_from_slice(&ia[..HANDSHAKE_LEN]);
        return inbound_after_known(
            stream,
            torrent,
            cfg,
            wire,
            peer_hs,
            crypto,
            ia[HANDSHAKE_LEN..].to_vec(),
        )
        .await;
    }

    let mut hs_buf = [0u8; HANDSHAKE_LEN];
    {
        let mut wire = wire;
        let dec = wire.as_mut().map(|w| &mut w.decrypt);
        net::read_exact_crypto_timeout(&mut stream, &mut hs_buf, dec, HS_IO).await?;
        if !looks_like_bt_handshake(&hs_buf) {
            return Err(Error::Msg("invalid handshake after PE".into()));
        }
        let (ih, _) = parse_handshake(&hs_buf)?;
        if ih != torrent.infohash {
            return Err(Error::Msg("handshake infohash mismatch after PE".into()));
        }
        inbound_after_known(stream, torrent, cfg, wire, hs_buf, crypto, Vec::new()).await
    }
}

async fn inbound_after_hs(
    stream: TcpStream,
    hs_buf: &[u8; HANDSHAKE_LEN],
    registry: Arc<parking_lot::RwLock<HotRegistry>>,
    cfg: PeerConfig,
    wire: Option<WireCrypto>,
    crypto: PeerCrypto,
) -> Result<InboundReady> {
    if !looks_like_bt_handshake(hs_buf) {
        return Err(Error::Msg("invalid handshake".into()));
    }
    let (infohash, _) = parse_handshake(hs_buf)?;
    let torrent = {
        let reg = registry.read();
        reg.get(&infohash)
            .ok_or_else(|| Error::Msg(format!("unknown infohash {}", hex::encode(infohash))))?
    };
    inbound_after_known(stream, torrent, cfg, wire, *hs_buf, crypto, Vec::new()).await
}

async fn inbound_after_known(
    mut stream: TcpStream,
    torrent: Arc<HotTorrent>,
    cfg: PeerConfig,
    mut wire: Option<WireCrypto>,
    peer_hs: [u8; HANDSHAKE_LEN],
    crypto: PeerCrypto,
    initial_plain: Vec<u8>,
) -> Result<InboundReady> {
    // Same as outbound: never run a peer session against our own peer_id.
    reject_if_self(&cfg.peer_id, &peer_hs)?;
    if let Some(cb) = cfg.on_bound.as_ref() {
        if !cb(torrent.id, torrent.name.clone()) {
            return Err(Error::Msg(format!(
                "torrent #{} at max_peers — rejecting inbound",
                torrent.id
            )));
        }
    }
    if let Some(ref a) = cfg.piece_count {
        a.store(torrent.piece_count, Ordering::Relaxed);
    }
    set_peer_crypto(&cfg.crypto, crypto);

    // Reply with our handshake before entering the established duplex loop.
    let our_hs = encode_handshake(&torrent.infohash, &cfg.peer_id);
    net::write_all_crypto(&mut stream, &our_hs, wire.as_mut().map(|w| &mut w.encrypt)).await?;

    Ok(InboundReady {
        stream,
        torrent,
        cfg,
        wire,
        peer_hs,
        initial_plain,
    })
}

async fn connect_peer(
    addr: std::net::SocketAddr,
    cfg: &PeerConfig,
    label: &str,
) -> Result<TcpStream> {
    let stream = timeout(Duration::from_secs(15), TcpStream::connect(addr))
        .await
        .map_err(|_| Error::Msg(format!("{label} connect timeout {addr}")))?
        .map_err(|e| Error::Msg(format!("{label} connect {addr}: {e}")))?;
    let _ = stream.set_nodelay(true);
    net::apply_socket_buffers(&stream, cfg.send_buffer_bytes, cfg.recv_buffer_bytes);
    Ok(stream)
}

/// Plain BT handshake (no MSE). Returns stream + peer handshake.
async fn dial_plain(
    addr: std::net::SocketAddr,
    torrent: &HotTorrent,
    cfg: &PeerConfig,
    our_hs: &[u8; HANDSHAKE_LEN],
) -> Result<(TcpStream, [u8; HANDSHAKE_LEN])> {
    let mut stream = connect_peer(addr, cfg, "plain").await?;
    net::write_all_crypto(&mut stream, our_hs, None).await?;
    let mut peer_hs = [0u8; HANDSHAKE_LEN];
    net::read_exact_crypto_timeout(&mut stream, &mut peer_hs, None, Duration::from_secs(12))
        .await?;
    if peer_hs[0] != 19 {
        // Common when remote requires MSE/PE: first byte is not BT protocol length.
        tracing::trace!(
            %addr,
            first = peer_hs[0],
            "outbound plain HS: non-BT first byte (peer may require encryption)"
        );
        return Err(Error::Msg(format!(
            "plain peer handshake invalid first={} (encryption required?)",
            peer_hs[0]
        )));
    }
    let (ih, _) = parse_handshake(&peer_hs)?;
    if ih != torrent.infohash {
        tracing::trace!(
            %addr,
            "outbound plain HS: infohash mismatch"
        );
        return Err(Error::Msg("infohash mismatch".into()));
    }
    Ok((stream, peer_hs))
}

/// MSE initiate + encrypted BT handshake. Publishes crypto on success.
async fn dial_pe(
    addr: std::net::SocketAddr,
    torrent: &HotTorrent,
    cfg: &PeerConfig,
    mode: EncryptionMode,
    our_hs: &[u8; HANDSHAKE_LEN],
) -> Result<(TcpStream, Option<WireCrypto>, [u8; HANDSHAKE_LEN])> {
    let mut stream = connect_peer(addr, cfg, "PE").await?;
    let mse = pe_initiate(&mut stream, &torrent.infohash, mode).await?;
    let (mut wire, crypto) = mse_to_wire(mse);
    set_peer_crypto(&cfg.crypto, crypto);
    net::write_all_crypto(&mut stream, our_hs, wire.as_mut().map(|w| &mut w.encrypt)).await?;
    let mut peer_hs = [0u8; HANDSHAKE_LEN];
    net::read_exact_crypto_timeout(
        &mut stream,
        &mut peer_hs,
        wire.as_mut().map(|w| &mut w.decrypt),
        Duration::from_secs(20),
    )
    .await?;
    if peer_hs[0] != 19 {
        tracing::trace!(
            %addr,
            first = peer_hs[0],
            ?mode,
            "outbound PE HS: non-BT first byte after MSE"
        );
        return Err(Error::Msg(format!(
            "PE peer handshake invalid first={}",
            peer_hs[0]
        )));
    }
    let (ih, _) = parse_handshake(&peer_hs)?;
    if ih != torrent.infohash {
        tracing::trace!(%addr, ?mode, "outbound PE HS: infohash mismatch");
        return Err(Error::Msg("infohash mismatch".into()));
    }
    Ok((stream, wire, peer_hs))
}

async fn pe_accept_incoming(
    stream: &mut TcpStream,
    first_byte: u8,
    registry: &parking_lot::RwLock<HotRegistry>,
    mode: EncryptionMode,
) -> Result<(Arc<HotTorrent>, MseSession, Vec<u8>)> {
    let mut ya = [0u8; DH_PUB_LEN];
    ya[0] = first_byte;
    // Must not use untimed read_exact — YA drip can hang for hours.
    net::read_exact_timeout(stream, &mut ya[1..], HS_IO).await?;

    let dh = DhKeyPair::generate();
    let yb = dh.public_key_bytes();
    net::write_all(stream, &yb).await?;

    let secret = dh.compute_secret(&ya)?;
    let req1 = hash_req1(&secret);
    const MAX: usize = 512 + 20 + 20 + 8 + 4 + 2 + 512 + 2 + 512;
    let mut buf = Vec::with_capacity(MAX);
    let mut scratch = Vec::with_capacity(4096);
    let req1_pos = loop {
        if buf.len() >= MAX {
            return Err(Error::Msg("MSE: req1 not found within pad limit".into()));
        }
        let n = net::read_some_timeout(stream, &mut scratch, 4096, HS_IO).await?;
        if n == 0 {
            return Err(Error::Msg("MSE: eof before req1".into()));
        }
        buf.extend_from_slice(&scratch[..n]);
        if let Some(pos) = find_slice(&buf, &req1) {
            break pos;
        }
    };

    let (torrent, provide, ia, dec, mut enc) = loop {
        if buf.len() < req1_pos + 40 {
            let n = net::read_some_timeout(stream, &mut scratch, 4096, HS_IO).await?;
            if n == 0 {
                return Err(Error::Msg("MSE: eof before req2".into()));
            }
            buf.extend_from_slice(&scratch[..n]);
            continue;
        }
        let mut obf = [0u8; 20];
        obf.copy_from_slice(&buf[req1_pos + 20..req1_pos + 40]);
        let req2 = deobfuscate_req2(&secret, &obf);
        let torrent = {
            let reg = registry.read();
            reg.match_req2(&req2)
                .ok_or_else(|| Error::Msg("MSE: unknown SKEY / torrent".into()))?
        };
        match receiver_parse_initiator(&secret, &torrent.infohash, &buf[req1_pos..]) {
            Ok((provide, ia, dec, enc)) => break (torrent, provide, ia, dec, enc),
            Err(e) if e.to_string().contains("truncated") || e.to_string().contains("overflow") => {
                let n = net::read_some_timeout(stream, &mut scratch, 4096, HS_IO).await?;
                if n == 0 {
                    return Err(Error::Msg(format!("MSE: eof mid-sync: {e}")));
                }
                buf.extend_from_slice(&scratch[..n]);
                if buf.len() > req1_pos + MAX {
                    return Err(Error::Msg(format!("MSE: sync too large: {e}")));
                }
            }
            Err(e) => return Err(e),
        }
    };

    let select = select_crypto(mode.crypto_provide_bits(), provide, mode)
        .ok_or_else(|| Error::Msg("MSE: no common crypto".into()))?;
    let resp = receiver_build_response(&mut enc, select);
    net::write_all(stream, &resp).await?;
    let rc4 = select & crate::crypto::CRYPTO_RC4 != 0;
    Ok((
        torrent,
        MseSession {
            crypto_select: select,
            encrypt: enc,
            decrypt: dec,
            rc4,
        },
        ia,
    ))
}

fn find_slice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Async PE initiator (scans PadB like the blocking path).
pub async fn pe_initiate(
    stream: &mut TcpStream,
    skey: &[u8; 20],
    mode: EncryptionMode,
) -> Result<MseSession> {
    use crate::crypto::{
        initiator_build_sync, initiator_scan_response, initiator_ya_pad, InitiatorResponseScan,
        MAX_PAD_B, MSE_RESP_HDR,
    };
    if !mode.wants_pe() {
        return Err(Error::Msg("PE initiate requires PE mode".into()));
    }
    let dh = DhKeyPair::generate();
    let pad_a = (rand::rng().next_u32() % 32) as usize;
    let flight1 = initiator_ya_pad(&dh, pad_a);
    net::write_all(stream, &flight1).await?;
    let mut yb = [0u8; DH_PUB_LEN];
    net::read_exact_timeout(stream, &mut yb, Duration::from_secs(20)).await?;
    let secret = dh.compute_secret(&yb)?;
    let provide = mode.crypto_provide_bits();
    let (sync, enc, mut dec) = initiator_build_sync(&secret, skey, provide, b"")?;
    net::write_all(stream, &sync).await?;

    let mut buf = Vec::with_capacity(MAX_PAD_B + MSE_RESP_HDR + 64);
    let mut scratch = Vec::with_capacity(256);
    let deadline = Instant::now() + Duration::from_secs(20);
    let select = loop {
        if Instant::now() > deadline {
            return Err(Error::Msg(
                "MSE: timeout waiting for peer ENCRYPT response".into(),
            ));
        }
        match initiator_scan_response(&mut dec, &buf)? {
            InitiatorResponseScan::Found { select, consumed } => {
                if consumed != buf.len() {
                    return Err(Error::Msg(format!(
                        "MSE: over-read after response (consumed={consumed} buf={})",
                        buf.len()
                    )));
                }
                break select;
            }
            InitiatorResponseScan::NeedMore => {
                if buf.len() >= MAX_PAD_B + MSE_RESP_HDR + MAX_PAD_B {
                    return Err(Error::Msg("MSE: response buffer full without VC".into()));
                }
                let n = net::read_some_timeout(stream, &mut scratch, 256, Duration::from_secs(2))
                    .await?;
                if n == 0 {
                    return Err(Error::Msg("MSE: eof before peer ENCRYPT response".into()));
                }
                buf.extend_from_slice(&scratch[..n]);
            }
        }
    };
    let rc4 = select & crate::crypto::CRYPTO_RC4 != 0;
    Ok(MseSession {
        crypto_select: select,
        encrypt: enc,
        decrypt: dec,
        rc4,
    })
}

#[cfg(test)]
mod self_peer_tests {
    use super::*;
    use crate::wire::encode_handshake;

    #[test]
    fn reject_matching_peer_id() {
        let id = *b"-sc0001-selftest!!!!"; // 20
        let ih = [0u8; 20];
        let hs = encode_handshake(&ih, &id);
        assert!(reject_if_self(&id, &hs).is_err());
        let other = *b"-sc0001-otherpeer!!!"; // 20
        let hs2 = encode_handshake(&ih, &other);
        assert!(reject_if_self(&id, &hs2).is_ok());
    }
}
