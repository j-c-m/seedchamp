//! Encryption policy (design K16).
//!
//! Names chosen for CLI/config clarity:
//! - `off` — plaintext only
//! - `prefer-plain` — PE allowed, pick plaintext when both offer it (default);
//!   outbound: plain first, PE retry if plain HS fails
//! - `prefer-rc4` — PE allowed, pick RC4 when both offer it;
//!   outbound: PE first, plain retry if PE/HS fails
//! - `require-rc4` — RC4 only

use std::fmt;
use std::str::FromStr;

use crate::error::{Error, Result};

pub const CRYPTO_PLAIN: u32 = 0x01;
pub const CRYPTO_RC4: u32 = 0x02;

/// Global PE / wire-crypto policy for seedchamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EncryptionMode {
    /// Plaintext BitTorrent only (no MSE/PE).
    Off,
    /// Offer plain|RC4; **select plaintext** when the peer allows it.
    #[default]
    PreferPlain,
    /// Offer plain|RC4; **select RC4** when the peer allows it (inbound).
    /// Outbound: PE first, plain BT retry if PE fails.
    PreferRc4,
    /// MSE + RC4 required; reject plain-only peers.
    RequireRc4,
}

impl EncryptionMode {
    /// Bits for MSE crypto_provide (CRYPTO_PLAIN=1, CRYPTO_RC4=2).
    pub fn crypto_provide_bits(self) -> u32 {
        match self {
            EncryptionMode::Off => CRYPTO_PLAIN,
            EncryptionMode::PreferPlain | EncryptionMode::PreferRc4 => CRYPTO_PLAIN | CRYPTO_RC4,
            EncryptionMode::RequireRc4 => CRYPTO_RC4,
        }
    }

    pub fn allows_plain(self) -> bool {
        !matches!(self, EncryptionMode::RequireRc4)
    }

    /// Whether to attempt / accept MSE negotiation.
    pub fn wants_pe(self) -> bool {
        !matches!(self, EncryptionMode::Off)
    }

    pub fn requires_rc4(self) -> bool {
        matches!(self, EncryptionMode::RequireRc4)
    }

    /// BEP 10 LTEP extended-handshake `e`.
    ///
    /// - `None` — omit key (plaintext-only / PE not allowed)
    /// - `Some(0)` — PE allowed, encrypted stream not required
    /// - `Some(1)` — require encrypted stream (RC4-only policy)
    pub fn ltep_e(self) -> Option<u8> {
        match self {
            EncryptionMode::Off => None,
            EncryptionMode::PreferPlain | EncryptionMode::PreferRc4 => Some(0),
            EncryptionMode::RequireRc4 => Some(1),
        }
    }

    /// Canonical config/CLI string.
    pub fn as_str(self) -> &'static str {
        match self {
            EncryptionMode::Off => "off",
            EncryptionMode::PreferPlain => "prefer-plain",
            EncryptionMode::PreferRc4 => "prefer-rc4",
            EncryptionMode::RequireRc4 => "require-rc4",
        }
    }
}

impl fmt::Display for EncryptionMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for EncryptionMode {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        let s = s.to_ascii_lowercase().replace('_', "-");
        match s.as_str() {
            "off" | "none" | "disable" | "disabled" | "0" => Ok(EncryptionMode::Off),
            "prefer-plain" | "prefer-plaintext" | "prefer_plain" | "plain" | "prefer" => {
                // bare "prefer" → prefer-plain
                Ok(EncryptionMode::PreferPlain)
            }
            "prefer-rc4" | "prefer-encrypted" | "prefer_rc4" | "rc4" => {
                Ok(EncryptionMode::PreferRc4)
            }
            "require-rc4" | "require" | "required" | "force" | "require_rc4" => {
                Ok(EncryptionMode::RequireRc4)
            }
            other => Err(Error::Msg(format!(
                "invalid encryption mode {other:?} (use off|prefer-plain|prefer-rc4|require-rc4)"
            ))),
        }
    }
}

/// Pick crypto_select given our provide bits, peer provide bits, and local preference.
pub fn select_crypto(our_provide: u32, peer_provide: u32, mode: EncryptionMode) -> Option<u32> {
    let common = our_provide & peer_provide;
    if common == 0 {
        return None;
    }
    match mode {
        EncryptionMode::Off => {
            if common & CRYPTO_PLAIN != 0 {
                Some(CRYPTO_PLAIN)
            } else {
                None
            }
        }
        EncryptionMode::PreferPlain => {
            if common & CRYPTO_PLAIN != 0 {
                Some(CRYPTO_PLAIN)
            } else if common & CRYPTO_RC4 != 0 {
                Some(CRYPTO_RC4)
            } else {
                None
            }
        }
        EncryptionMode::PreferRc4 => {
            if common & CRYPTO_RC4 != 0 {
                Some(CRYPTO_RC4)
            } else if common & CRYPTO_PLAIN != 0 {
                Some(CRYPTO_PLAIN)
            } else {
                None
            }
        }
        EncryptionMode::RequireRc4 => {
            if common & CRYPTO_RC4 != 0 {
                Some(CRYPTO_RC4)
            } else {
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_mode() {
        assert_eq!(
            "prefer-plain".parse::<EncryptionMode>().unwrap(),
            EncryptionMode::PreferPlain
        );
        assert_eq!(
            "prefer".parse::<EncryptionMode>().unwrap(),
            EncryptionMode::PreferPlain
        );
        assert_eq!(
            "prefer-rc4".parse::<EncryptionMode>().unwrap(),
            EncryptionMode::PreferRc4
        );
        assert_eq!(
            "require-rc4".parse::<EncryptionMode>().unwrap(),
            EncryptionMode::RequireRc4
        );
        assert_eq!(
            "require".parse::<EncryptionMode>().unwrap(),
            EncryptionMode::RequireRc4
        );
    }

    #[test]
    fn select_prefer_plain_hits_plain() {
        let s = select_crypto(
            CRYPTO_PLAIN | CRYPTO_RC4,
            CRYPTO_PLAIN | CRYPTO_RC4,
            EncryptionMode::PreferPlain,
        );
        assert_eq!(s, Some(CRYPTO_PLAIN));
    }

    #[test]
    fn select_prefer_rc4_hits_rc4() {
        let s = select_crypto(
            CRYPTO_PLAIN | CRYPTO_RC4,
            CRYPTO_PLAIN | CRYPTO_RC4,
            EncryptionMode::PreferRc4,
        );
        assert_eq!(s, Some(CRYPTO_RC4));
    }

    #[test]
    fn select_require_rc4() {
        assert_eq!(
            select_crypto(
                CRYPTO_RC4,
                CRYPTO_PLAIN | CRYPTO_RC4,
                EncryptionMode::RequireRc4
            ),
            Some(CRYPTO_RC4)
        );
        assert_eq!(
            select_crypto(CRYPTO_RC4, CRYPTO_PLAIN, EncryptionMode::RequireRc4),
            None
        );
    }

    #[test]
    fn default_is_prefer_plain() {
        assert_eq!(EncryptionMode::default(), EncryptionMode::PreferPlain);
    }

    #[test]
    fn ltep_e_matches_bep10() {
        assert_eq!(EncryptionMode::Off.ltep_e(), None);
        assert_eq!(EncryptionMode::PreferPlain.ltep_e(), Some(0));
        assert_eq!(EncryptionMode::PreferRc4.ltep_e(), Some(0));
        assert_eq!(EncryptionMode::RequireRc4.ltep_e(), Some(1));
    }
}
