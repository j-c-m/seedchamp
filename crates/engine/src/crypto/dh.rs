//! Diffie–Hellman for MSE (768-bit prime, generator 2).

use num_bigint::BigUint;
use num_traits::Zero;
use rand::Rng;

use crate::error::{Error, Result};

/// MSE prime (same as libtorrent / Azureus MSE).
pub const DH_PRIME: [u8; 96] = [
    0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xC9, 0x0F, 0xDA, 0xA2, 0x21, 0x68, 0xC2, 0x34,
    0xC4, 0xC6, 0x62, 0x8B, 0x80, 0xDC, 0x1C, 0xD1, 0x29, 0x02, 0x4E, 0x08, 0x8A, 0x67, 0xCC, 0x74,
    0x02, 0x0B, 0xBE, 0xA6, 0x3B, 0x13, 0x9B, 0x22, 0x51, 0x4A, 0x08, 0x79, 0x8E, 0x34, 0x04, 0xDD,
    0xEF, 0x95, 0x19, 0xB3, 0xCD, 0x3A, 0x43, 0x1B, 0x30, 0x2B, 0x0A, 0x6D, 0xF2, 0x5F, 0x14, 0x37,
    0x4F, 0xE1, 0x35, 0x6D, 0x6D, 0x51, 0xC2, 0x45, 0xE4, 0x85, 0xB5, 0x76, 0x62, 0x5E, 0x7E, 0xC6,
    0xF4, 0x4C, 0x42, 0xE9, 0xA6, 0x3A, 0x36, 0x21, 0x00, 0x00, 0x00, 0x00, 0x00, 0x09, 0x05, 0x63,
];

pub const DH_PUB_LEN: usize = 96;

/// Ephemeral DH keypair for one MSE handshake.
pub struct DhKeyPair {
    private: BigUint,
    public: BigUint,
    prime: BigUint,
}

impl DhKeyPair {
    pub fn generate() -> Self {
        let prime = BigUint::from_bytes_be(&DH_PRIME);
        let g = BigUint::from(2u32);
        // Private exponent: 160 random bits (common MSE practice).
        let mut raw = [0u8; 20];
        rand::rng().fill_bytes(&mut raw);
        let private = BigUint::from_bytes_be(&raw);
        let public = g.modpow(&private, &prime);
        Self {
            private,
            public,
            prime,
        }
    }

    /// Public key as 96-byte big-endian, left-padded with zeros (libtorrent store_pub_key).
    pub fn public_key_bytes(&self) -> [u8; DH_PUB_LEN] {
        pad_left_96(&self.public.to_bytes_be())
    }

    /// Shared secret S, 96-byte big-endian left-padded.
    pub fn compute_secret(&self, peer_public: &[u8]) -> Result<[u8; DH_PUB_LEN]> {
        if peer_public.len() != DH_PUB_LEN {
            return Err(Error::Msg(format!(
                "DH peer public length {} != {DH_PUB_LEN}",
                peer_public.len()
            )));
        }
        let ya = BigUint::from_bytes_be(peer_public);
        if ya.is_zero() || ya >= self.prime {
            return Err(Error::Msg("invalid DH peer public key".into()));
        }
        let s = ya.modpow(&self.private, &self.prime);
        Ok(pad_left_96(&s.to_bytes_be()))
    }
}

fn pad_left_96(bytes: &[u8]) -> [u8; DH_PUB_LEN] {
    let mut out = [0u8; DH_PUB_LEN];
    if bytes.len() >= DH_PUB_LEN {
        out.copy_from_slice(&bytes[bytes.len() - DH_PUB_LEN..]);
    } else {
        out[DH_PUB_LEN - bytes.len()..].copy_from_slice(bytes);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dh_shared_secret_matches() {
        let a = DhKeyPair::generate();
        let b = DhKeyPair::generate();
        let sa = a.compute_secret(&b.public_key_bytes()).unwrap();
        let sb = b.compute_secret(&a.public_key_bytes()).unwrap();
        assert_eq!(sa, sb);
        assert!(!sa.iter().all(|&x| x == 0));
    }
}
