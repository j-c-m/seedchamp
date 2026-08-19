//! Established peer: inbound BT frame parse/dispatch and [`EstablishedPeer`].
//!
//! Full-duplex socket loops live in [`super::duplex`]. Process-wide Compio
//! threads live in [`crate::runtime`].

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use compio::net::TcpStream;
use flume::Sender as FlumeSender;

use crate::error::{Error, Result};
use crate::upload::{classify_upload_request, UploadBlock, UploadRequestStatus};
use crate::wire::{
    allowed_fast_for_addr, apply_have_all_none, encode_allowed_fast_messages,
    encode_extended_handshake, encode_message, encode_possession_fast,
    handshake_supports_extensions, handshake_supports_fast, identify_peer_id, ltep_client_version,
    parse_handshake, parse_message, prefer_client_label, FastSession, Message, ParsedMessage,
    ALLOWED_FAST_K, EXT_HANDSHAKE, HANDSHAKE_LEN,
};

use super::super::net;
use super::config::PeerConfig;
use super::download::PeerDownload;
use super::helpers::{clear_dl_queue, publish_peer_choking, publish_peer_have, WireCrypto};
use crate::hot::HotTorrent;
use crate::runtime::HashOutcome;

/// Bound pure-leech exit work (hash outcomes + Cancel/NotInterested flush).
pub(crate) const PURE_LEECH_EXIT_TIMEOUT: Duration = Duration::from_secs(5);

/// Parse and handle every complete BT frame currently in `read_buf`.
/// Returns whether the request pipeline should refill.
pub(crate) fn parse_available_messages(
    read_buf: &mut net::ReadCursor,
    dl: &mut PeerDownload,
    out: &mut impl super::duplex::PeerOut,
    fast: &mut FastSession,
    peer_interested: &mut bool,
    last_piece_at: &mut Instant,
    last_useful_at: &mut Instant,
    torrent: &HotTorrent,
    cfg: &PeerConfig,
    downloading: bool,
    hash_tx: &FlumeSender<HashOutcome>,
) -> Result<bool> {
    let mut need_fill = false;
    // PIECE body borrows unparsed; handle + advance before await/compact.
    loop {
        let Some((frame, used)) = parse_message(read_buf.unparsed())? else {
            break;
        };
        match frame {
            ParsedMessage::Piece {
                index,
                begin,
                block,
            } => {
                // Empty body (message_len == 9): ignore.
                if block.is_empty() {
                    // fall through
                } else if downloading {
                    // Stall clock only on ingested blocks. Discarded PIECE
                    // (already have / not staged) must not postpone Cancel.
                    if !torrent.has_piece(index) {
                        let Some(ref hash_pool) = cfg.hash else {
                            return Err(Error::Msg("hash thread not configured".into()));
                        };
                        if dl.handle_piece(hash_tx, hash_pool, index, begin, block)? {
                            let now = Instant::now();
                            *last_piece_at = now;
                            *last_useful_at = now;
                            if let Some(ref c) = cfg.wire_down {
                                c.fetch_add(block.len() as u64, Ordering::Relaxed);
                            }
                        }
                        need_fill = true;
                    }
                }
            }
            ParsedMessage::Msg(msg) => match msg {
                Message::KeepAlive => {}
                Message::Choke => {
                    dl.peer_choking = true;
                    publish_peer_choking(cfg, true);
                    // Outstanding Requests stay as-is (seeder may still serve
                    // until it processes choke; we do not re-issue).
                }
                Message::Unchoke => {
                    dl.peer_choking = false;
                    publish_peer_choking(cfg, false);
                    if downloading {
                        *last_piece_at = Instant::now();
                        need_fill = true;
                    }
                }
                Message::Interested => {
                    *peer_interested = true;
                    if let Some(ref a) = cfg.peer_interested {
                        a.store(true, Ordering::Relaxed);
                    }
                }
                Message::NotInterested => {
                    *peer_interested = false;
                    if let Some(ref a) = cfg.peer_interested {
                        a.store(false, Ordering::Relaxed);
                    }
                    out.clear_pieces();
                }
                Message::Have(i) => {
                    if (i as usize) / 8 < dl.peer_bf.len() {
                        if let Some(ref mut a) = dl.peer_avail {
                            a.on_have(i);
                        }
                        dl.peer_bf[(i as usize) / 8] |= 1 << (7 - (i as usize % 8));
                        publish_peer_have(cfg, &dl.peer_bf, torrent.piece_count);
                        if downloading {
                            need_fill = true;
                        }
                    }
                }
                Message::HaveAll => {
                    apply_have_all_none(&mut dl.peer_bf, torrent.piece_count, true);
                    if let Some(ref mut a) = dl.peer_avail {
                        a.on_bitfield(&dl.peer_bf);
                    }
                    publish_peer_have(cfg, &dl.peer_bf, torrent.piece_count);
                    if downloading {
                        need_fill = true;
                    }
                }
                Message::HaveNone => {
                    apply_have_all_none(&mut dl.peer_bf, torrent.piece_count, false);
                    if let Some(ref mut a) = dl.peer_avail {
                        a.on_bitfield(&dl.peer_bf);
                    }
                    publish_peer_have(cfg, &dl.peer_bf, torrent.piece_count);
                }
                Message::SuggestPiece(i) => {
                    if downloading {
                        dl.push_suggest(i);
                        if dl.can_request() {
                            need_fill = true;
                        }
                    }
                }
                Message::AllowedFast(i) => {
                    if downloading {
                        fast.on_allowed_fast(i);
                        dl.allowed_fast.insert(i);
                        need_fill = true;
                    }
                }
                Message::RejectRequest {
                    index,
                    begin,
                    length,
                } => {
                    if downloading {
                        let _ = dl.staging.clear_request(index, begin, length);
                        need_fill = true;
                    }
                }
                Message::Bitfield(bf) => {
                    let n = dl.peer_bf.len().min(bf.len());
                    dl.peer_bf[..n].copy_from_slice(&bf[..n]);
                    if let Some(ref mut a) = dl.peer_avail {
                        a.on_bitfield(&dl.peer_bf);
                    }
                    publish_peer_have(cfg, &dl.peer_bf, torrent.piece_count);
                    if downloading {
                        need_fill = true;
                    }
                }
                Message::Request {
                    index,
                    begin,
                    length,
                } if cfg.allow_upload => {
                    let plen = torrent.layout().piece_size(index).unwrap_or(0);
                    let status = classify_upload_request(
                        torrent.piece_count,
                        plen,
                        torrent.has_piece(index),
                        *peer_interested,
                        index,
                        begin,
                        length,
                    );
                    if status == UploadRequestStatus::Accept {
                        // Writer enforces queue limits / Reject when full.
                        let _ = out.try_push_piece(UploadBlock {
                            index,
                            begin,
                            length,
                        });
                    } else if fast.enabled {
                        let frame = {
                            let s = out.ctrl_scratch();
                            s.clear();
                            s.append_reject_request(index, begin, length);
                            s.take()
                        };
                        out.push_ctrl_owned(frame);
                    }
                }
                Message::Cancel {
                    index,
                    begin,
                    length,
                } if cfg.allow_upload => {
                    let _ = out.cancel_piece(UploadBlock {
                        index,
                        begin,
                        length,
                    });
                }
                Message::Extended { ext_id, payload } if ext_id == EXT_HANDSHAKE => {
                    if let Some(v) = ltep_client_version(&payload) {
                        if let Some(slot) = cfg.client_label.as_ref() {
                            let mut g = slot.lock();
                            *g = prefer_client_label(&g, Some(&v));
                        }
                    }
                }
                // Wire PIECE never lands here (see ParsedMessage::Piece).
                _ => {}
            },
        }
        read_buf.advance(used);
    }
    Ok(need_fill)
}

/// Established connection + negotiated capabilities; owns the main I/O loop.
pub(crate) struct EstablishedPeer {
    stream: TcpStream,
    torrent: Arc<HotTorrent>,
    cfg: PeerConfig,
    wire: Option<WireCrypto>,
    peer_supports_ext: bool,
    peer_supports_fast: bool,
    /// Remainder after PE IA (bytes already decrypted past the BT handshake).
    initial_plain: Vec<u8>,
}

impl EstablishedPeer {
    pub(crate) fn new(
        stream: TcpStream,
        torrent: Arc<HotTorrent>,
        cfg: PeerConfig,
        wire: Option<WireCrypto>,
        peer_hs: &[u8],
        initial_plain: &[u8],
    ) -> Self {
        let peer_supports_ext = if peer_hs.len() >= HANDSHAKE_LEN {
            handshake_supports_extensions(&peer_hs[..HANDSHAKE_LEN])
        } else {
            false
        };
        let peer_supports_fast = if peer_hs.len() >= HANDSHAKE_LEN {
            handshake_supports_fast(&peer_hs[..HANDSHAKE_LEN])
        } else {
            false
        };
        // Best-effort client from Azureus/Shadow peer_id; LTEP `v` may upgrade later.
        if let Some(slot) = cfg.client_label.as_ref() {
            if peer_hs.len() >= HANDSHAKE_LEN {
                if let Ok((_, pid)) = parse_handshake(&peer_hs[..HANDSHAKE_LEN]) {
                    *slot.lock() = identify_peer_id(&pid);
                }
            }
        }
        Self {
            stream,
            torrent,
            cfg,
            wire,
            peer_supports_ext,
            peer_supports_fast,
            initial_plain: initial_plain.to_vec(),
        }
    }

    pub(crate) async fn run(mut self) -> Result<()> {
        let torrent = Arc::clone(&self.torrent);
        let cfg = self.cfg.clone();
        let peer_addr = self.stream.peer_addr().ok();
        // Consume the stream — no leftover clone (into_split is clone + self).
        let (rd, mut wr) = self.stream.into_split();
        let mut wire = self.wire.take();
        let peer_supports_ext = self.peer_supports_ext;
        let peer_supports_fast = self.peer_supports_fast;

        let want_download =
            cfg.allow_download && cfg.hash.is_some() && !torrent.is_download_complete();

        let have_rx = torrent.subscribe_have();
        let mut allowed_to_peer = std::collections::HashSet::new();
        {
            let bf = torrent.bitfield_snapshot();
            let mut out = if peer_supports_fast {
                encode_possession_fast(torrent.have_count(), torrent.piece_count, bf)
            } else {
                encode_message(&Message::Bitfield(bf))
            };
            if want_download {
                out.extend_from_slice(&encode_message(&Message::Interested));
            }
            if cfg.allow_upload {
                out.extend_from_slice(&encode_message(&Message::Unchoke));
            }
            if peer_supports_fast {
                if let Some(addr) = peer_addr {
                    let set = allowed_fast_for_addr(
                        ALLOWED_FAST_K,
                        torrent.piece_count,
                        &torrent.infohash,
                        addr,
                    );
                    allowed_to_peer = set.iter().copied().collect();
                    out.extend_from_slice(&encode_allowed_fast_messages(&set));
                }
            }
            if peer_supports_ext {
                out.extend_from_slice(&encode_extended_handshake(
                    &cfg.ltep_client,
                    cfg.listen_port,
                    cfg.encryption.ltep_e(),
                ));
            }
            net::write_all_crypto(&mut wr, &out, wire.as_mut().map(|w| &mut w.encrypt)).await?;
            let mut batch = Vec::new();
            while let Ok(index) = have_rx.try_recv() {
                batch.extend_from_slice(&encode_message(&Message::Have(index)));
            }
            if !batch.is_empty() {
                net::write_all_crypto(&mut wr, &batch, wire.as_mut().map(|w| &mut w.encrypt))
                    .await?;
            }
        }

        let (encrypt, decrypt) = match wire {
            Some(w) => (Some(w.encrypt), Some(w.decrypt)),
            None => (None, None),
        };
        let initial_plain = std::mem::take(&mut self.initial_plain);

        let r = super::duplex::run_duplex(
            rd,
            wr,
            torrent,
            cfg,
            encrypt,
            decrypt,
            initial_plain,
            peer_supports_fast,
            allowed_to_peer,
            have_rx,
        )
        .await;
        clear_dl_queue(&self.cfg);
        r
    }
}
