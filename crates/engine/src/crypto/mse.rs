//! Message Stream Encryption handshake (initiator + receiver).
//!
//! Completes PE against a peer speaking the same protocol (loopback tests).
//! Live TCP path: `peer` (async inbound/outbound).
//!
//! RC4 keystreams are continuous: handshake ENCRYPT/DECRYPT and later BT
//! payload share the same cipher state (1024-byte discard only at creation).

use rand::Rng;

#[cfg(test)]
use super::config::CRYPTO_PLAIN;
use super::config::{select_crypto, EncryptionMode, CRYPTO_RC4};
use super::dh::{DhKeyPair, DH_PUB_LEN};
use super::keys::{hash_req1, hash_req2, hash_req3, obfuscated_hash, sha1_salt2};
use super::rc4::Rc4;
use crate::error::{Error, Result};

pub const VC: [u8; 8] = [0; 8];

/// Result of a finished MSE handshake (ready for BT payload).
#[derive(Clone)]
pub struct MseSession {
    pub crypto_select: u32,
    pub encrypt: Rc4,
    pub decrypt: Rc4,
    /// True if RC4 was selected (payload must be encrypted after MSE).
    pub rc4: bool,
}

/// Build initiator's first flight: YA (96) || padA.
pub fn initiator_ya_pad(dh: &DhKeyPair, pad_len: usize) -> Vec<u8> {
    let pad_len = pad_len.min(512);
    let mut out = Vec::with_capacity(DH_PUB_LEN + pad_len);
    out.extend_from_slice(&dh.public_key_bytes());
    let mut pad = vec![0u8; pad_len];
    rand::rng().fill_bytes(&mut pad);
    out.extend_from_slice(&pad);
    out
}

fn rc4_key_a(secret: &[u8], skey: &[u8; 20]) -> Rc4 {
    Rc4::new_mse(&sha1_salt2(b"keyA", secret, skey))
}

fn rc4_key_b(secret: &[u8], skey: &[u8; 20]) -> Rc4 {
    Rc4::new_mse(&sha1_salt2(b"keyB", secret, skey))
}

/// Initiator: HASH(req1)||obfuscated req2||ENCRYPT_keyA(VC||provide||padC||IA).
/// Returns (wire_bytes, encrypt_stream_keyA, decrypt_stream_keyB).
pub fn initiator_build_sync(
    secret: &[u8; DH_PUB_LEN],
    skey: &[u8; 20],
    provide: u32,
    ia: &[u8],
) -> Result<(Vec<u8>, Rc4, Rc4)> {
    let mut enc = rc4_key_a(secret, skey);
    let dec = rc4_key_b(secret, skey);

    let mut inner = Vec::new();
    inner.extend_from_slice(&VC);
    inner.extend_from_slice(&provide.to_be_bytes());
    inner.extend_from_slice(&0u16.to_be_bytes()); // padC = 0
    inner.extend_from_slice(&(ia.len() as u16).to_be_bytes());
    inner.extend_from_slice(ia);
    enc.crypt_inplace(&mut inner);

    let mut out = Vec::new();
    out.extend_from_slice(&hash_req1(secret));
    out.extend_from_slice(&obfuscated_hash(secret, skey));
    out.extend_from_slice(&inner);
    Ok((out, enc, dec))
}

/// Receiver parses initiator sync; returns (provide, ia, decrypt_keyA, encrypt_keyB).
pub fn receiver_parse_initiator(
    secret: &[u8; DH_PUB_LEN],
    skey: &[u8; 20],
    buf: &[u8],
) -> Result<(u32, Vec<u8>, Rc4, Rc4)> {
    let req1 = hash_req1(secret);
    let pos = find_slice(buf, &req1).ok_or_else(|| Error::Msg("MSE req1 not found".into()))?;
    let base = pos + 20;
    if buf.len() < base + 20 + 8 + 4 + 2 + 2 {
        return Err(Error::Msg("MSE initiator payload truncated".into()));
    }
    let mut obf = [0u8; 20];
    obf.copy_from_slice(&buf[base..base + 20]);
    let mut got_req2 = [0u8; 20];
    let r3 = hash_req3(secret);
    for i in 0..20 {
        got_req2[i] = obf[i] ^ r3[i];
    }
    if got_req2 != hash_req2(skey) {
        return Err(Error::Msg("MSE req2/SKEY mismatch".into()));
    }

    let mut dec = rc4_key_a(secret, skey);
    let enc = rc4_key_b(secret, skey);

    let enc_start = base + 20;
    let mut plain = buf[enc_start..].to_vec();
    dec.crypt_inplace(&mut plain);

    if plain.len() < 8 + 4 + 2 + 2 {
        return Err(Error::Msg("MSE decrypt short".into()));
    }
    if plain[..8] != VC {
        return Err(Error::Msg("MSE VC mismatch".into()));
    }
    let provide = u32::from_be_bytes(plain[8..12].try_into().unwrap());
    let pad_c = u16::from_be_bytes(plain[12..14].try_into().unwrap()) as usize;
    let mut o = 14 + pad_c;
    if plain.len() < o + 2 {
        return Err(Error::Msg("MSE padC overflow".into()));
    }
    let ia_len = u16::from_be_bytes(plain[o..o + 2].try_into().unwrap()) as usize;
    o += 2;
    if plain.len() < o + ia_len {
        return Err(Error::Msg("MSE IA overflow".into()));
    }
    let ia = plain[o..o + ia_len].to_vec();
    Ok((provide, ia, dec, enc))
}

/// Receiver encrypts response with keyB stream (continues after this for BT encrypt).
pub fn receiver_build_response(enc: &mut Rc4, select: u32) -> Vec<u8> {
    let mut inner = Vec::new();
    inner.extend_from_slice(&VC);
    inner.extend_from_slice(&select.to_be_bytes());
    inner.extend_from_slice(&0u16.to_be_bytes());
    enc.crypt_inplace(&mut inner);
    inner
}

/// Max plaintext PadB between peer YB and ENCRYPT(VC‖select‖padD) (MSE / libtorrent).
pub const MAX_PAD_B: usize = 512;
/// ENCRYPT header: VC(8) + crypto_select(4) + len(padD)(2).
pub const MSE_RESP_HDR: usize = 8 + 4 + 2;

/// Result of scanning for ENCRYPT(VC‖select‖padD) after optional PadB.
#[derive(Debug)]
pub enum InitiatorResponseScan {
    /// Found at `consumed` bytes from start of `buf`; `dec` advanced past header+padD.
    Found { select: u32, consumed: usize },
    /// Need more bytes (PadB / padD incomplete).
    NeedMore,
}

/// Find peer ENCRYPT response after optional plaintext PadB (0..=512).
///
/// `dec` is the keyB stream **before** any response bytes. On [`Found`], `dec` is
/// advanced through the encrypted header and padD (ready for BT payload).
///
/// Used when initiating PE against stock libtorrent/rtorrent (random PadB).
pub fn initiator_scan_response(dec: &mut Rc4, buf: &[u8]) -> Result<InitiatorResponseScan> {
    if buf.len() < MSE_RESP_HDR {
        return Ok(InitiatorResponseScan::NeedMore);
    }
    let max_off = buf.len().saturating_sub(MSE_RESP_HDR).min(MAX_PAD_B);
    let mut saw_incomplete = false;
    for off in 0..=max_off {
        let mut trial = dec.clone();
        let mut hdr = buf[off..off + MSE_RESP_HDR].to_vec();
        trial.crypt_inplace(&mut hdr);
        if hdr[..8] != VC {
            continue;
        }
        let select = u32::from_be_bytes(hdr[8..12].try_into().unwrap());
        let pad_d = u16::from_be_bytes(hdr[12..14].try_into().unwrap()) as usize;
        if pad_d > MAX_PAD_B {
            // Invalid padD at this offset — keep scanning (false VC collide rare).
            continue;
        }
        let need = off + MSE_RESP_HDR + pad_d;
        if buf.len() < need {
            // Possible match but padD not fully buffered yet; try other offsets first.
            saw_incomplete = true;
            continue;
        }
        // Commit keystream through header + padD at the chosen offset.
        let mut commit = buf[off..need].to_vec();
        dec.crypt_inplace(&mut commit);
        if commit[..8] != VC {
            return Err(Error::Msg("MSE response VC commit mismatch".into()));
        }
        return Ok(InitiatorResponseScan::Found {
            select,
            consumed: need,
        });
    }
    if saw_incomplete {
        return Ok(InitiatorResponseScan::NeedMore);
    }
    if buf.len() >= MAX_PAD_B + MSE_RESP_HDR {
        return Err(Error::Msg(
            "MSE response VC not found within PadB limit".into(),
        ));
    }
    Ok(InitiatorResponseScan::NeedMore)
}

/// Run full MSE between two local parties.
pub fn handshake_loopback(
    skey: &[u8; 20],
    initiator_mode: EncryptionMode,
    receiver_mode: EncryptionMode,
) -> Result<(MseSession, MseSession)> {
    if !initiator_mode.wants_pe() || !receiver_mode.wants_pe() {
        return Err(Error::Msg("loopback PE requires both sides want PE".into()));
    }

    let dh_i = DhKeyPair::generate();
    let dh_r = DhKeyPair::generate();
    let s_i = dh_i.compute_secret(&dh_r.public_key_bytes())?;
    let s_r = dh_r.compute_secret(&dh_i.public_key_bytes())?;
    assert_eq!(s_i, s_r);
    let secret = s_i;

    let provide_i = initiator_mode.crypto_provide_bits();
    let (sync, i_enc, mut i_dec) = initiator_build_sync(&secret, skey, provide_i, b"")?;

    let (provide_peer, _ia, r_dec, mut r_enc) = receiver_parse_initiator(&secret, skey, &sync)?;

    let select = select_crypto(
        receiver_mode.crypto_provide_bits(),
        provide_peer,
        receiver_mode,
    )
    .ok_or_else(|| Error::Msg("receiver rejected crypto".into()))?;

    let resp = receiver_build_response(&mut r_enc, select);
    let select2 = match initiator_scan_response(&mut i_dec, &resp)? {
        InitiatorResponseScan::Found { select, .. } => select,
        InitiatorResponseScan::NeedMore => {
            return Err(Error::Msg("MSE loopback response incomplete".into()));
        }
    };
    assert_eq!(select, select2);

    let rc4 = select & CRYPTO_RC4 != 0;
    // After MSE, if plain selected, further BT is not RC4-encrypted (streams unused for crypto).
    // If RC4 selected, continue the same keystreams for BT.
    Ok((
        MseSession {
            crypto_select: select,
            encrypt: i_enc,
            decrypt: i_dec,
            rc4,
        },
        MseSession {
            crypto_select: select,
            encrypt: r_enc,
            decrypt: r_dec,
            rc4,
        },
    ))
}

fn find_slice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_response_with_pad_b() {
        let secret = [0xABu8; DH_PUB_LEN];
        let skey = [0xCDu8; 20];
        let select = CRYPTO_RC4;
        let mut enc = rc4_key_b(&secret, &skey);
        let resp = receiver_build_response(&mut enc, select);
        assert_eq!(resp.len(), MSE_RESP_HDR);

        // Prepend random PadB like libtorrent.
        let pad_b: Vec<u8> = (0..137u8).collect();
        let mut wire = pad_b.clone();
        wire.extend_from_slice(&resp);

        let mut dec = rc4_key_b(&secret, &skey);
        match initiator_scan_response(&mut dec, &wire).unwrap() {
            InitiatorResponseScan::Found {
                select: got,
                consumed,
            } => {
                assert_eq!(got, select);
                assert_eq!(consumed, pad_b.len() + MSE_RESP_HDR);
            }
            InitiatorResponseScan::NeedMore => panic!("expected Found"),
        }
    }

    #[test]
    fn scan_response_pad_b_zero() {
        let secret = [0x11u8; DH_PUB_LEN];
        let skey = [0x22u8; 20];
        let mut enc = rc4_key_b(&secret, &skey);
        let resp = receiver_build_response(&mut enc, CRYPTO_PLAIN);
        let mut dec = rc4_key_b(&secret, &skey);
        match initiator_scan_response(&mut dec, &resp).unwrap() {
            InitiatorResponseScan::Found { select, consumed } => {
                assert_eq!(select, CRYPTO_PLAIN);
                assert_eq!(consumed, MSE_RESP_HDR);
            }
            InitiatorResponseScan::NeedMore => panic!("expected Found"),
        }
    }

    #[test]
    fn loopback_prefer_rc4_stream() {
        let skey = [0x11u8; 20];
        let (mut a, mut b) =
            handshake_loopback(&skey, EncryptionMode::PreferRc4, EncryptionMode::PreferRc4)
                .unwrap();
        assert!(a.rc4);
        let mut msg = b"HAVE all the pieces!!".to_vec();
        let orig = msg.clone();
        a.encrypt.crypt_inplace(&mut msg);
        b.decrypt.crypt_inplace(&mut msg);
        assert_eq!(msg, orig);
        let mut msg2 = b"BITFIELD...".to_vec();
        let orig2 = msg2.clone();
        b.encrypt.crypt_inplace(&mut msg2);
        a.decrypt.crypt_inplace(&mut msg2);
        assert_eq!(msg2, orig2);
    }

    #[test]
    fn loopback_prefer_plain_selects_plain() {
        let skey = [0x33u8; 20];
        let (a, _) = handshake_loopback(
            &skey,
            EncryptionMode::PreferPlain,
            EncryptionMode::PreferPlain,
        )
        .unwrap();
        assert!(!a.rc4);
        assert_eq!(a.crypto_select, CRYPTO_PLAIN);
    }

    #[test]
    fn loopback_require_rc4() {
        let skey = [0x22u8; 20];
        let (a, _) = handshake_loopback(
            &skey,
            EncryptionMode::RequireRc4,
            EncryptionMode::RequireRc4,
        )
        .unwrap();
        assert!(a.rc4);
        assert_eq!(a.crypto_select, CRYPTO_RC4);
    }

    #[test]
    fn provide_bits() {
        assert_eq!(EncryptionMode::Off.crypto_provide_bits(), CRYPTO_PLAIN);
        assert_eq!(
            EncryptionMode::PreferPlain.crypto_provide_bits(),
            CRYPTO_PLAIN | CRYPTO_RC4
        );
        assert_eq!(
            EncryptionMode::PreferRc4.crypto_provide_bits(),
            CRYPTO_PLAIN | CRYPTO_RC4
        );
        assert_eq!(EncryptionMode::RequireRc4.crypto_provide_bits(), CRYPTO_RC4);
    }
}
