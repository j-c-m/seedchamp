//! BitTorrent wire messages (BEP 3 + BEP 6 Fast + BEP 10 LTEP).

use crate::error::{Error, Result};

pub const BT_PROTOCOL: &[u8] = b"BitTorrent protocol";
pub const HANDSHAKE_LEN: usize = 68;

pub const MSG_CHOKE: u8 = 0;
pub const MSG_UNCHOKE: u8 = 1;
pub const MSG_INTERESTED: u8 = 2;
pub const MSG_NOT_INTERESTED: u8 = 3;
pub const MSG_HAVE: u8 = 4;
pub const MSG_BITFIELD: u8 = 5;
pub const MSG_REQUEST: u8 = 6;
pub const MSG_PIECE: u8 = 7;
pub const MSG_CANCEL: u8 = 8;
/// BEP 6 Fast Extension.
pub const MSG_SUGGEST: u8 = 0x0d;
pub const MSG_HAVE_ALL: u8 = 0x0e;
pub const MSG_HAVE_NONE: u8 = 0x0f;
pub const MSG_REJECT: u8 = 0x10;
pub const MSG_ALLOWED_FAST: u8 = 0x11;
/// BEP 10 extension protocol message id.
pub const MSG_EXTENDED: u8 = 20;
/// Extended message id 0 = handshake.
pub const EXT_HANDSHAKE: u8 = 0;
/// LTEP `reqq` we advertise (aligned with download `MAX_PIPELINE` = 8192).
pub const LTEP_REQQ: u32 = 8192;
/// Max BT message length field (id + payload), matching libtorrent / libtorrent-rasterbar
/// (`1 << 20` / 1 MiB). Reject before buffering so a hostile length cannot OOM `read_buf`.
pub const MAX_MESSAGE_LENGTH: usize = 1 << 20;
/// PIECE header on the wire: `u32 len` + `u8 id` + `u32 index` + `u32 begin` (no body).
pub const SIZEOF_PIECE: usize = 13;
/// Reserved-bytes index for the extension-protocol bit (libtorrent convention).
const HS_EXT_BYTE: usize = 25; // handshake[20+5]
const HS_EXT_BIT: u8 = 0x10;
/// BEP 6 Fast: third least significant bit of last reserved byte (handshake[27]).
const HS_FAST_BYTE: usize = 27; // handshake[20+7]
const HS_FAST_BIT: u8 = 0x04;

/// Peer message (encode + non-PIECE inbound parse).
///
/// Inbound **PIECE** is special-cased: [`parse_message`] returns
/// [`ParsedMessage::Piece`] with a borrow into the read buffer (no `to_vec`).
#[derive(Debug, Clone)]
pub enum Message {
    KeepAlive,
    Choke,
    Unchoke,
    Interested,
    NotInterested,
    Have(u32),
    Bitfield(Vec<u8>),
    Request {
        index: u32,
        begin: u32,
        length: u32,
    },
    /// Owned PIECE (encode / tests). Wire parse uses [`ParsedMessage::Piece`].
    Piece {
        index: u32,
        begin: u32,
        block: Vec<u8>,
    },
    Cancel {
        index: u32,
        begin: u32,
        length: u32,
    },
    /// BEP 6: advisory piece suggestion.
    SuggestPiece(u32),
    /// BEP 6: complete seed (replaces full bitfield).
    HaveAll,
    /// BEP 6: empty leecher (replaces empty bitfield).
    HaveNone,
    /// BEP 6: request will not be satisfied.
    RejectRequest {
        index: u32,
        begin: u32,
        length: u32,
    },
    /// BEP 6: piece may be requested while choked.
    AllowedFast(u32),
    /// BEP 10: first payload byte is extension message id, rest is payload.
    Extended {
        ext_id: u8,
        payload: Vec<u8>,
    },
    Unknown(u8, Vec<u8>),
}

/// Result of [`parse_message`]: control messages owned, PIECE payload borrowed.
#[derive(Debug)]
pub enum ParsedMessage<'a> {
    /// Non-PIECE (or keep-alive): fully owned [`Message`].
    Msg(Message),
    /// PIECE body is a slice into the peer read buffer — handle before compact/await.
    Piece {
        index: u32,
        begin: u32,
        block: &'a [u8],
    },
}

pub fn encode_handshake(infohash: &[u8; 20], peer_id: &[u8; 20]) -> [u8; HANDSHAKE_LEN] {
    let mut buf = [0u8; HANDSHAKE_LEN];
    buf[0] = 19;
    buf[1..20].copy_from_slice(BT_PROTOCOL);
    // BEP 10 extension protocol (libtorrent: reserved[5] |= 0x10).
    buf[HS_EXT_BYTE] |= HS_EXT_BIT;
    // BEP 6 Fast Extension (reserved[7] |= 0x04).
    buf[HS_FAST_BYTE] |= HS_FAST_BIT;
    buf[28..48].copy_from_slice(infohash);
    buf[48..68].copy_from_slice(peer_id);
    buf
}

/// True when the peer handshake reserved field advertises BEP 10.
pub fn handshake_supports_extensions(hs: &[u8]) -> bool {
    hs.len() > HS_EXT_BYTE && (hs[HS_EXT_BYTE] & HS_EXT_BIT) != 0
}

/// True when the peer handshake reserved field advertises BEP 6 Fast.
pub fn handshake_supports_fast(hs: &[u8]) -> bool {
    hs.len() > HS_FAST_BYTE && (hs[HS_FAST_BYTE] & HS_FAST_BIT) != 0
}

/// BEP 10 extended handshake (keys we support).
///
/// - `e` — optional PE preference (`0`/`1`); omit when PE is off
/// - `m` — extension map (empty; no PEX/metadata)
/// - `p` — our listen port
/// - `reqq` — max upload request queue ([`LTEP_REQQ`])
/// - `v` — client version string
///
/// Keys are emitted in sorted order for a stable fingerprint.
pub fn encode_extended_handshake(client: &str, listen_port: u16, e: Option<u8>) -> Vec<u8> {
    // d [1:eiNe] 1:mde 1:piNe 4:reqqiNe 1:vN:<client> e
    let mut benc = Vec::with_capacity(80 + client.len());
    benc.push(b'd');
    if let Some(ev) = e {
        benc.extend_from_slice(format!("1:ei{ev}e").as_bytes());
    }
    benc.extend_from_slice(b"1:mde");
    benc.extend_from_slice(format!("1:pi{listen_port}e").as_bytes());
    benc.extend_from_slice(format!("4:reqqi{LTEP_REQQ}e").as_bytes());
    benc.extend_from_slice(format!("1:v{}:", client.len()).as_bytes());
    benc.extend_from_slice(client.as_bytes());
    benc.push(b'e');
    encode_message(&Message::Extended {
        ext_id: EXT_HANDSHAKE,
        payload: benc,
    })
}

pub fn parse_handshake(buf: &[u8]) -> Result<([u8; 20], [u8; 20])> {
    if buf.len() < HANDSHAKE_LEN {
        return Err(Error::Msg("handshake truncated".into()));
    }
    if buf[0] != 19 || &buf[1..20] != BT_PROTOCOL {
        return Err(Error::Msg("not a BitTorrent handshake".into()));
    }
    let mut ih = [0u8; 20];
    let mut pid = [0u8; 20];
    ih.copy_from_slice(&buf[28..48]);
    pid.copy_from_slice(&buf[48..68]);
    Ok((ih, pid))
}

pub fn looks_like_bt_handshake(buf: &[u8]) -> bool {
    buf.len() >= 20 && buf[0] == 19 && &buf[1..20] == BT_PROTOCOL
}

/// Encode length-prefixed message (length includes id+payload).
pub fn encode_message(msg: &Message) -> Vec<u8> {
    match msg {
        Message::KeepAlive => vec![0, 0, 0, 0],
        Message::Choke => encode_id(MSG_CHOKE, &[]),
        Message::Unchoke => encode_id(MSG_UNCHOKE, &[]),
        Message::Interested => encode_id(MSG_INTERESTED, &[]),
        Message::NotInterested => encode_id(MSG_NOT_INTERESTED, &[]),
        Message::Have(i) => encode_id(MSG_HAVE, &i.to_be_bytes()),
        Message::Bitfield(bf) => encode_id(MSG_BITFIELD, bf),
        Message::Request {
            index,
            begin,
            length,
        } => {
            let mut out = Vec::with_capacity(WIRE_REQUEST);
            append_request(&mut out, *index, *begin, *length);
            out
        }
        Message::Piece {
            index,
            begin,
            block,
        } => {
            let mut p = Vec::with_capacity(8 + block.len());
            p.extend_from_slice(&index.to_be_bytes());
            p.extend_from_slice(&begin.to_be_bytes());
            p.extend_from_slice(block);
            encode_id(MSG_PIECE, &p)
        }
        Message::Cancel {
            index,
            begin,
            length,
        } => {
            let mut out = Vec::with_capacity(WIRE_REQUEST);
            append_cancel(&mut out, *index, *begin, *length);
            out
        }
        Message::SuggestPiece(i) => encode_id(MSG_SUGGEST, &i.to_be_bytes()),
        Message::HaveAll => encode_id(MSG_HAVE_ALL, &[]),
        Message::HaveNone => encode_id(MSG_HAVE_NONE, &[]),
        Message::RejectRequest {
            index,
            begin,
            length,
        } => {
            let mut out = Vec::with_capacity(WIRE_REQUEST);
            append_reject_request(&mut out, *index, *begin, *length);
            out
        }
        Message::AllowedFast(i) => encode_id(MSG_ALLOWED_FAST, &i.to_be_bytes()),
        Message::Extended { ext_id, payload } => {
            let mut p = Vec::with_capacity(1 + payload.len());
            p.push(*ext_id);
            p.extend_from_slice(payload);
            encode_id(MSG_EXTENDED, &p)
        }
        Message::Unknown(id, payload) => encode_id(*id, payload),
    }
}

fn encode_id(id: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + 1 + payload.len());
    append_id(&mut out, id, payload);
    out
}

/// Wire sizes for fixed control messages (length prefix + body).
pub const WIRE_KEEPALIVE: usize = 4;
pub const WIRE_ID_ONLY: usize = 5; // choke / unchoke / interested / …
pub const WIRE_HAVE: usize = 9;
pub const WIRE_REQUEST: usize = 17; // also Cancel / RejectRequest

/// Append a length-prefixed id+payload frame into `out` (no intermediate alloc).
#[inline]
pub fn append_id(out: &mut Vec<u8>, id: u8, payload: &[u8]) {
    let len = (1 + payload.len()) as u32;
    out.reserve(4 + 1 + payload.len());
    out.extend_from_slice(&len.to_be_bytes());
    out.push(id);
    out.extend_from_slice(payload);
}

#[inline]
pub fn append_keepalive(out: &mut Vec<u8>) {
    out.extend_from_slice(&[0, 0, 0, 0]);
}

#[inline]
pub fn append_interested(out: &mut Vec<u8>) {
    append_id(out, MSG_INTERESTED, &[]);
}

#[inline]
pub fn append_not_interested(out: &mut Vec<u8>) {
    append_id(out, MSG_NOT_INTERESTED, &[]);
}

#[inline]
pub fn append_have(out: &mut Vec<u8>, index: u32) {
    append_id(out, MSG_HAVE, &index.to_be_bytes());
}

#[inline]
pub fn append_request(out: &mut Vec<u8>, index: u32, begin: u32, length: u32) {
    let mut p = [0u8; 12];
    p[0..4].copy_from_slice(&index.to_be_bytes());
    p[4..8].copy_from_slice(&begin.to_be_bytes());
    p[8..12].copy_from_slice(&length.to_be_bytes());
    append_id(out, MSG_REQUEST, &p);
}

#[inline]
pub fn append_cancel(out: &mut Vec<u8>, index: u32, begin: u32, length: u32) {
    let mut p = [0u8; 12];
    p[0..4].copy_from_slice(&index.to_be_bytes());
    p[4..8].copy_from_slice(&begin.to_be_bytes());
    p[8..12].copy_from_slice(&length.to_be_bytes());
    append_id(out, MSG_CANCEL, &p);
}

#[inline]
pub fn append_reject_request(out: &mut Vec<u8>, index: u32, begin: u32, length: u32) {
    let mut p = [0u8; 12];
    p[0..4].copy_from_slice(&index.to_be_bytes());
    p[4..8].copy_from_slice(&begin.to_be_bytes());
    p[8..12].copy_from_slice(&length.to_be_bytes());
    append_id(out, MSG_REJECT, &p);
}

/// Parsed 13-byte PIECE header (body not included).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PieceHeader {
    pub index: u32,
    pub begin: u32,
    /// Block payload length (`message_len - 9`).
    pub block_len: u32,
    pub message_len: u32,
}

/// Parse PIECE header when `buf` has ≥ [`SIZEOF_PIECE`] bytes.
/// `Ok(None)` if shorter, or not a PIECE (caller uses large fill + [`parse_message`]).
pub fn parse_piece_header(buf: &[u8]) -> Result<Option<PieceHeader>> {
    if buf.len() < SIZEOF_PIECE {
        return Ok(None);
    }
    let message_len = u32::from_be_bytes(buf[0..4].try_into().unwrap());
    if message_len == 0 {
        return Ok(None);
    }
    if message_len as usize > MAX_MESSAGE_LENGTH {
        return Err(Error::Msg(format!(
            "BT message length {message_len} exceeds max {MAX_MESSAGE_LENGTH}"
        )));
    }
    if buf[4] != MSG_PIECE {
        return Ok(None);
    }
    if message_len < 9 {
        return Err(Error::Msg("bad PIECE length".into()));
    }
    Ok(Some(PieceHeader {
        index: u32::from_be_bytes(buf[5..9].try_into().unwrap()),
        begin: u32::from_be_bytes(buf[9..13].try_into().unwrap()),
        block_len: message_len - 9,
        message_len,
    }))
}

/// Parse one inbound message; returns `(frame, bytes_consumed)`.
///
/// **PIECE only:** body is borrowed (`ParsedMessage::Piece`) — no `to_vec`.
/// Everything else is owned [`Message`].
///
/// On a complete 4-byte length prefix, rejects `len > `[`MAX_MESSAGE_LENGTH`]
/// immediately so callers stop reading.
pub fn parse_message(buf: &[u8]) -> Result<Option<(ParsedMessage<'_>, usize)>> {
    if buf.len() < 4 {
        return Ok(None);
    }
    let len = u32::from_be_bytes(buf[0..4].try_into().unwrap()) as usize;
    if len == 0 {
        return Ok(Some((ParsedMessage::Msg(Message::KeepAlive), 4)));
    }
    // Cap before waiting for `4 + len` bytes (DoS: peer advertises ~2^32).
    if len > MAX_MESSAGE_LENGTH {
        return Err(Error::Msg(format!(
            "BT message length {len} exceeds max {MAX_MESSAGE_LENGTH}"
        )));
    }
    if buf.len() < 4 + len {
        return Ok(None);
    }
    let id = buf[4];
    let payload = &buf[5..4 + len];
    // Hot path: PIECE payload stays in the read buffer (one copy later in staging).
    if id == MSG_PIECE {
        if payload.len() < 8 {
            return Err(Error::Msg("bad PIECE".into()));
        }
        return Ok(Some((
            ParsedMessage::Piece {
                index: u32::from_be_bytes(payload[0..4].try_into().unwrap()),
                begin: u32::from_be_bytes(payload[4..8].try_into().unwrap()),
                block: &payload[8..],
            },
            4 + len,
        )));
    }
    let msg = match id {
        MSG_CHOKE => Message::Choke,
        MSG_UNCHOKE => Message::Unchoke,
        MSG_INTERESTED => Message::Interested,
        MSG_NOT_INTERESTED => Message::NotInterested,
        MSG_HAVE => {
            if payload.len() != 4 {
                return Err(Error::Msg("bad HAVE".into()));
            }
            Message::Have(u32::from_be_bytes(payload.try_into().unwrap()))
        }
        MSG_BITFIELD => Message::Bitfield(payload.to_vec()),
        MSG_REQUEST => {
            if payload.len() != 12 {
                return Err(Error::Msg("bad REQUEST".into()));
            }
            Message::Request {
                index: u32::from_be_bytes(payload[0..4].try_into().unwrap()),
                begin: u32::from_be_bytes(payload[4..8].try_into().unwrap()),
                length: u32::from_be_bytes(payload[8..12].try_into().unwrap()),
            }
        }
        MSG_CANCEL => {
            if payload.len() != 12 {
                return Err(Error::Msg("bad CANCEL".into()));
            }
            Message::Cancel {
                index: u32::from_be_bytes(payload[0..4].try_into().unwrap()),
                begin: u32::from_be_bytes(payload[4..8].try_into().unwrap()),
                length: u32::from_be_bytes(payload[8..12].try_into().unwrap()),
            }
        }
        MSG_SUGGEST => {
            if payload.len() != 4 {
                return Err(Error::Msg("bad SUGGEST".into()));
            }
            Message::SuggestPiece(u32::from_be_bytes(payload.try_into().unwrap()))
        }
        MSG_HAVE_ALL => {
            if !payload.is_empty() {
                return Err(Error::Msg("bad HAVE_ALL".into()));
            }
            Message::HaveAll
        }
        MSG_HAVE_NONE => {
            if !payload.is_empty() {
                return Err(Error::Msg("bad HAVE_NONE".into()));
            }
            Message::HaveNone
        }
        MSG_REJECT => {
            if payload.len() != 12 {
                return Err(Error::Msg("bad REJECT".into()));
            }
            Message::RejectRequest {
                index: u32::from_be_bytes(payload[0..4].try_into().unwrap()),
                begin: u32::from_be_bytes(payload[4..8].try_into().unwrap()),
                length: u32::from_be_bytes(payload[8..12].try_into().unwrap()),
            }
        }
        MSG_ALLOWED_FAST => {
            if payload.len() != 4 {
                return Err(Error::Msg("bad ALLOWED_FAST".into()));
            }
            Message::AllowedFast(u32::from_be_bytes(payload.try_into().unwrap()))
        }
        MSG_EXTENDED => {
            if payload.is_empty() {
                return Err(Error::Msg("bad EXTENDED".into()));
            }
            Message::Extended {
                ext_id: payload[0],
                payload: payload[1..].to_vec(),
            }
        }
        other => Message::Unknown(other, payload.to_vec()),
    };
    Ok(Some((ParsedMessage::Msg(msg), 4 + len)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handshake_roundtrip() {
        let ih = [1u8; 20];
        let pid = [2u8; 20];
        let hs = encode_handshake(&ih, &pid);
        let (a, b) = parse_handshake(&hs).unwrap();
        assert_eq!(a, ih);
        assert_eq!(b, pid);
        assert!(looks_like_bt_handshake(&hs));
        assert!(handshake_supports_extensions(&hs), "we advertise BEP 10");
        assert!(handshake_supports_fast(&hs), "we advertise BEP 6 Fast");
    }

    #[test]
    fn fast_messages_roundtrip() {
        for m in [
            Message::HaveAll,
            Message::HaveNone,
            Message::SuggestPiece(42),
            Message::AllowedFast(7),
            Message::RejectRequest {
                index: 1,
                begin: 0,
                length: 16384,
            },
        ] {
            let enc = encode_message(&m);
            let (parsed, n) = parse_message(&enc).unwrap().unwrap();
            assert_eq!(n, enc.len());
            let ParsedMessage::Msg(ref got) = parsed else {
                panic!("expected Msg, got {parsed:?}");
            };
            match (&m, got) {
                (Message::HaveAll, Message::HaveAll) => {}
                (Message::HaveNone, Message::HaveNone) => {}
                (Message::SuggestPiece(a), Message::SuggestPiece(b)) => assert_eq!(a, b),
                (Message::AllowedFast(a), Message::AllowedFast(b)) => assert_eq!(a, b),
                (
                    Message::RejectRequest {
                        index: i1,
                        begin: b1,
                        length: l1,
                    },
                    Message::RejectRequest {
                        index: i2,
                        begin: b2,
                        length: l2,
                    },
                ) => {
                    assert_eq!(i1, i2);
                    assert_eq!(b1, b2);
                    assert_eq!(l1, l2);
                }
                _ => panic!("mismatch {m:?} vs {got:?}"),
            }
        }
    }

    #[test]
    fn extended_handshake_reqq_p_e() {
        let enc = encode_extended_handshake("seedchamp 0.1.0", 6881, Some(0));
        let (parsed, n) = parse_message(&enc).unwrap().unwrap();
        assert_eq!(n, enc.len());
        match parsed {
            ParsedMessage::Msg(Message::Extended { ext_id, payload }) => {
                assert_eq!(ext_id, EXT_HANDSHAKE);
                let s = std::str::from_utf8(&payload).unwrap();
                assert!(s.contains("reqq"), "{s}");
                assert!(s.contains(&LTEP_REQQ.to_string()), "{s}");
                assert!(s.contains("1:pi6881e"), "{s}");
                assert!(s.contains("1:ei0e"), "{s}");
                assert!(s.contains("seedchamp 0.1.0"), "{s}");
                // Sorted key order: e, m, p, reqq, v
                let e_at = s.find("1:e").expect("e");
                let m_at = s.find("1:m").expect("m");
                let p_at = s.find("1:p").expect("p");
                assert!(e_at < m_at && m_at < p_at, "{s}");
            }
            other => panic!("expected Extended, got {other:?}"),
        }
        // encryption off → no e key
        let plain = encode_extended_handshake("seedchamp 0.1.0", 51413, None);
        let (parsed, _) = parse_message(&plain).unwrap().unwrap();
        match parsed {
            ParsedMessage::Msg(Message::Extended { payload, .. }) => {
                let s = std::str::from_utf8(&payload).unwrap();
                assert!(!s.contains("1:ei"), "{s}");
                assert!(s.contains("1:pi51413e"), "{s}");
            }
            other => panic!("expected Extended, got {other:?}"),
        }
    }

    #[test]
    fn request_roundtrip() {
        let m = Message::Request {
            index: 3,
            begin: 16384,
            length: 16384,
        };
        let enc = encode_message(&m);
        let (parsed, n) = parse_message(&enc).unwrap().unwrap();
        assert_eq!(n, enc.len());
        match parsed {
            ParsedMessage::Msg(Message::Request {
                index,
                begin,
                length,
            }) => {
                assert_eq!(index, 3);
                assert_eq!(begin, 16384);
                assert_eq!(length, 16384);
            }
            _ => panic!("wrong msg"),
        }
    }

    #[test]
    fn piece_parse_borrows_without_alloc() {
        let block = vec![0xABu8; 16384];
        let m = Message::Piece {
            index: 9,
            begin: 32768,
            block: block.clone(),
        };
        let enc = encode_message(&m);
        let (parsed, n) = parse_message(&enc).unwrap().unwrap();
        assert_eq!(n, enc.len());
        match parsed {
            ParsedMessage::Piece {
                index,
                begin,
                block: b,
            } => {
                assert_eq!(index, 9);
                assert_eq!(begin, 32768);
                assert_eq!(b, block.as_slice());
                let start = b.as_ptr() as usize;
                let enc_start = enc.as_ptr() as usize;
                assert!(start >= enc_start && start < enc_start + enc.len());
            }
            other => panic!("expected ParsedMessage::Piece, got {other:?}"),
        }
    }

    #[test]
    fn rejects_oversized_length_without_full_body() {
        // Only the 4-byte length prefix is present; body never arrives.
        let mut hdr = (MAX_MESSAGE_LENGTH as u32 + 1).to_be_bytes().to_vec();
        assert!(parse_message(&hdr)
            .unwrap_err()
            .to_string()
            .contains("exceeds max"));

        // Exactly at the cap with incomplete body → need more data, not error.
        hdr = (MAX_MESSAGE_LENGTH as u32).to_be_bytes().to_vec();
        assert!(matches!(parse_message(&hdr), Ok(None)));

        // Just over cap with a full (fake) frame still errors on length first.
        let over = MAX_MESSAGE_LENGTH as u32 + 1;
        let mut big = over.to_be_bytes().to_vec();
        big.push(MSG_CHOKE);
        big.resize(4 + over as usize, 0);
        assert!(parse_message(&big)
            .unwrap_err()
            .to_string()
            .contains("exceeds max"));
    }

    #[test]
    fn accepts_message_at_max_length() {
        // len = MAX_MESSAGE_LENGTH means 1 id byte + (MAX-1) payload.
        let len = MAX_MESSAGE_LENGTH as u32;
        let mut buf = len.to_be_bytes().to_vec();
        buf.push(MSG_BITFIELD);
        buf.resize(4 + MAX_MESSAGE_LENGTH, 0xab);
        let (msg, n) = parse_message(&buf).unwrap().unwrap();
        assert_eq!(n, 4 + MAX_MESSAGE_LENGTH);
        match msg {
            ParsedMessage::Msg(Message::Bitfield(bf)) => {
                assert_eq!(bf.len(), MAX_MESSAGE_LENGTH - 1)
            }
            other => panic!("expected Bitfield, got {other:?}"),
        }
    }

    /// 13-byte header for the direct-to-staging path (body not present).
    fn piece_header_bytes(index: u32, begin: u32, block_len: u32) -> [u8; SIZEOF_PIECE] {
        let message_len = 9u32 + block_len;
        let mut out = [0u8; SIZEOF_PIECE];
        out[0..4].copy_from_slice(&message_len.to_be_bytes());
        out[4] = MSG_PIECE;
        out[5..9].copy_from_slice(&index.to_be_bytes());
        out[9..13].copy_from_slice(&begin.to_be_bytes());
        out
    }

    #[test]
    fn parse_piece_header_happy_and_boundaries() {
        let h = piece_header_bytes(9, 32768, 16384);
        let got = parse_piece_header(&h).unwrap().unwrap();
        assert_eq!(got.index, 9);
        assert_eq!(got.begin, 32768);
        assert_eq!(got.block_len, 16384);
        assert_eq!(got.message_len, 9 + 16384);

        // Empty body still a valid PIECE header (message_len == 9).
        let empty = piece_header_bytes(0, 0, 0);
        let got = parse_piece_header(&empty).unwrap().unwrap();
        assert_eq!(got.block_len, 0);
        assert_eq!(got.message_len, 9);

        // Short buffer / non-PIECE / KeepAlive → None (fallback path).
        assert!(matches!(
            parse_piece_header(&h[..SIZEOF_PIECE - 1]),
            Ok(None)
        ));
        let choke = {
            let mut b = piece_header_bytes(0, 0, 0);
            b[4] = MSG_CHOKE;
            b[0..4].copy_from_slice(&1u32.to_be_bytes());
            b
        };
        assert!(matches!(parse_piece_header(&choke), Ok(None)));
        let keepalive = [0u8; SIZEOF_PIECE];
        assert!(matches!(parse_piece_header(&keepalive), Ok(None)));
    }

    #[test]
    fn parse_piece_header_rejects_bad_lengths() {
        // Oversized length (DoS guard) — same cap as parse_message.
        let mut over = piece_header_bytes(0, 0, 0);
        over[0..4].copy_from_slice(&(MAX_MESSAGE_LENGTH as u32 + 1).to_be_bytes());
        assert!(parse_piece_header(&over)
            .unwrap_err()
            .to_string()
            .contains("exceeds max"));

        // PIECE id with message_len < 9 (not enough for index+begin).
        let mut short = piece_header_bytes(0, 0, 0);
        short[0..4].copy_from_slice(&8u32.to_be_bytes());
        short[4] = MSG_PIECE;
        assert!(parse_piece_header(&short)
            .unwrap_err()
            .to_string()
            .contains("bad PIECE"));
    }
}
