//! MSE key derivation (libtorrent-compatible sha1_salt).

use sha1::{Digest, Sha1};

use super::rc4::Rc4;

/// HASH(salt || key)
pub fn sha1_salt1(salt: &[u8], key: &[u8]) -> [u8; 20] {
    let mut h = Sha1::new();
    h.update(salt);
    h.update(key);
    let d = h.finalize();
    let mut out = [0u8; 20];
    out.copy_from_slice(&d);
    out
}

/// HASH(salt || key1 || key2)
pub fn sha1_salt2(salt: &[u8], key1: &[u8], key2: &[u8]) -> [u8; 20] {
    let mut h = Sha1::new();
    h.update(salt);
    h.update(key1);
    h.update(key2);
    let d = h.finalize();
    let mut out = [0u8; 20];
    out.copy_from_slice(&d);
    out
}

/// HASH('req1', S) — sync point for initiator detection of peer response region.
pub fn hash_req1(secret: &[u8]) -> [u8; 20] {
    sha1_salt1(b"req1", secret)
}

/// HASH('req2', SKEY) — torrent identity in MSE.
pub fn hash_req2(skey: &[u8; 20]) -> [u8; 20] {
    sha1_salt1(b"req2", skey)
}

/// HASH('req3', S)
pub fn hash_req3(secret: &[u8]) -> [u8; 20] {
    sha1_salt1(b"req3", secret)
}

/// Obfuscated handshake torrent hash: HASH('req2', SKEY) XOR HASH('req3', S)
pub fn obfuscated_hash(secret: &[u8], skey: &[u8; 20]) -> [u8; 20] {
    let a = hash_req2(skey);
    let b = hash_req3(secret);
    let mut out = [0u8; 20];
    for i in 0..20 {
        out[i] = a[i] ^ b[i];
    }
    out
}

/// Recover SKEY material XOR form: deobfuscate with HASH('req3', S).
pub fn deobfuscate_req2(secret: &[u8], obfuscated: &[u8; 20]) -> [u8; 20] {
    let r3 = hash_req3(secret);
    let mut out = [0u8; 20];
    for i in 0..20 {
        out[i] = obfuscated[i] ^ r3[i];
    }
    out
}

/// RC4 encrypt/decrypt for one peer after MSE, matching libtorrent:
/// - incoming: decrypt=keyA, encrypt=keyB
/// - outgoing: decrypt=keyB, encrypt=keyA
/// Each stream discards 1024 bytes.
pub fn derive_peer_rc4(secret: &[u8], skey: &[u8; 20], incoming: bool) -> (Rc4, Rc4) {
    // encrypt, decrypt
    let (enc_salt, dec_salt) = if incoming {
        (&b"keyB"[..], &b"keyA"[..])
    } else {
        (&b"keyA"[..], &b"keyB"[..])
    };
    let enc_key = sha1_salt2(enc_salt, secret, skey);
    let dec_key = sha1_salt2(dec_salt, secret, skey);
    (Rc4::new_mse(&enc_key), Rc4::new_mse(&dec_key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn req_hashes_stable() {
        let s = [1u8; 96];
        let skey = [2u8; 20];
        assert_ne!(hash_req1(&s), hash_req3(&s));
        let o = obfuscated_hash(&s, &skey);
        assert_eq!(deobfuscate_req2(&s, &o), hash_req2(&skey));
    }

    #[test]
    fn peer_rc4_cross() {
        let s = [7u8; 96];
        let skey = [9u8; 20];
        // A is incoming, B is outgoing relative to A
        let (mut a_enc, mut a_dec) = derive_peer_rc4(&s, &skey, true);
        let (mut b_enc, mut b_dec) = derive_peer_rc4(&s, &skey, false);

        let mut msg = b"bitfield payload".to_vec();
        let orig = msg.clone();
        a_enc.crypt_inplace(&mut msg);
        b_dec.crypt_inplace(&mut msg);
        assert_eq!(msg, orig);

        let mut msg2 = b"request piece".to_vec();
        let orig2 = msg2.clone();
        b_enc.crypt_inplace(&mut msg2);
        a_dec.crypt_inplace(&mut msg2);
        assert_eq!(msg2, orig2);
    }
}
