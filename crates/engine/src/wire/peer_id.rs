//! Map BitTorrent peer_id (20 bytes) to a human-readable client label.
//!
//! Conventions: Azureus-style (`-XXyyyy-…`), Shadow-style, Mainline `M…`.
//! Best-effort and spoofable — LTEP extended-handshake `v` is preferred when present.

use crate::bencode;

/// Identify a remote client from its 20-byte handshake peer_id.
pub fn identify_peer_id(peer_id: &[u8; 20]) -> String {
    if let Some(s) = azureus_style(peer_id) {
        return s;
    }
    if let Some(s) = mainline_style(peer_id) {
        return s;
    }
    if let Some(s) = shadow_style(peer_id) {
        return s;
    }
    printable_fallback(peer_id)
}

/// Extract BEP 10 extended-handshake `v` (client version string) if present.
pub fn ltep_client_version(payload: &[u8]) -> Option<String> {
    let (val, _) = bencode::decode(payload).ok()?;
    let v = val.dict_get_str("v")?.trim();
    if v.is_empty() {
        return None;
    }
    // Cap for TUI / memory.
    let s: String = v.chars().take(64).collect();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// LTEP `v` when present; else peer_id label.
pub fn prefer_client_label(from_peer_id: &str, from_ltep: Option<&str>) -> String {
    match from_ltep {
        Some(v) if !v.trim().is_empty() => v.trim().chars().take(64).collect(),
        _ => from_peer_id.to_string(),
    }
}

fn azureus_style(id: &[u8; 20]) -> Option<String> {
    // -XXYYYY- + 12 random
    if id[0] != b'-' || id[7] != b'-' {
        return None;
    }
    let code = std::str::from_utf8(&id[1..3]).ok()?;
    if !code.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'~') {
        return None;
    }
    let ver = &id[3..7];
    let name = azureus_client_name(code)?;
    Some(format_with_version(name, ver))
}

fn azureus_client_name(code: &str) -> Option<&'static str> {
    // Common codes (theory.org / BEP 20 / popular clients). Case-sensitive codes.
    Some(match code {
        "7T" => "aTorrent",
        "AB" => "AnyEvent::BitTorrent",
        "AG" | "A~" => "Ares",
        "AR" => "Arctic",
        "AT" => "Artemis",
        "AV" => "Avicora",
        "AX" => "BitPump",
        "AZ" => "Azureus",
        "BB" => "BitBuddy",
        "BC" => "BitComet",
        "BE" => "BitTorrent SDK",
        "BF" => "Bitflu",
        "BG" => "BTG",
        "BL" => "BitBlinder",
        "BP" => "BitTorrent Pro",
        "BR" => "BitRocket",
        "BS" => "BTSlave",
        "BT" => "Mainline",
        "BW" => "BitWombat",
        "BX" => "Bittorrent X",
        "CD" => "Enhanced CTorrent",
        "CT" => "CTorrent",
        "DE" => "Deluge",
        "DP" => "Propagate Data Client",
        "EB" => "EBit",
        "ES" => "electric sheep",
        "FC" => "FileCroc",
        "FD" => "Free Download Manager",
        "FT" => "FoxTorrent",
        "FX" => "Freebox BitTorrent",
        "GS" => "GSTorrent",
        "HK" => "Hekate",
        "HL" => "Halite",
        "HM" => "hMule",
        "HN" => "Hydranode",
        "IL" => "iLivid",
        "JS" => "Justseed.it",
        "JT" => "JavaTorrent",
        "KG" => "KGet",
        "KT" => "KTorrent",
        "LC" => "LeechCraft",
        "LH" => "LH-ABC",
        "LP" => "Lphant",
        "LT" | "lt" => "libtorrent",
        "LW" => "LimeWire",
        "MK" => "Meerkat",
        "MO" => "MonoTorrent",
        "MP" => "MooPolice",
        "MR" => "Miro",
        "MT" => "MoonlightTorrent",
        "NB" => "Net::BitTorrent",
        "NX" => "Net Transport",
        "OS" => "OneSwarm",
        "OT" => "OmegaTorrent",
        "PB" => "Protocol::BitTorrent",
        "PD" => "Pando",
        "PI" => "PicoTorrent",
        "PT" => "PHPTracker",
        "qB" => "qBittorrent",
        "QD" => "QQDownload",
        "QT" => "Qt 4 Torrent",
        "RT" => "Retriever",
        "RZ" => "RezTorrent",
        "S~" => "Shareaza alpha/beta",
        "SB" => "SwiftBit",
        "SD" => "Xunlei",
        "SM" => "SoMud",
        "SP" => "BitSpirit",
        "SS" => "SwarmScope",
        "ST" => "SymTorrent",
        "st" => "sharktorrent",
        "SZ" => "Shareaza",
        "TB" => "Torch",
        "TE" => "terasaur Seed Bank",
        "TL" => "Tribler",
        "TN" => "Torrent.NET",
        "TR" => "Transmission",
        "TS" => "TorrentStorm",
        "TT" => "TuoTu",
        "UL" => "uLeecher!",
        "UM" => "µTorrent Mac",
        "UT" => "µTorrent",
        "VG" => "Vagaa",
        "WD" => "WebTorrent Desktop",
        "WT" => "BitLet",
        "WW" => "WebTorrent",
        "WY" => "FireTorrent",
        "XL" => "Xunlei",
        "XT" => "XanTorrent",
        "XX" => "Xtorrent",
        "ZT" => "ZipTorrent",
        // seedchamp / custom
        "sc" | "SC" => "seedchamp",
        _ => return None,
    })
}

fn format_with_version(name: &str, ver: &[u8]) -> String {
    if ver.len() == 4 && ver.iter().all(|b| b.is_ascii_digit()) {
        let a = ver[0] - b'0';
        let b = ver[1] - b'0';
        let c = ver[2] - b'0';
        let d = ver[3] - b'0';
        // Drop trailing zeros: 4600 → 4.6.0, 0615 → 0.6.1.5 style abbreviated.
        if d == 0 {
            if c == 0 {
                format!("{name} {a}.{b}")
            } else {
                format!("{name} {a}.{b}.{c}")
            }
        } else {
            format!("{name} {a}.{b}.{c}.{d}")
        }
    } else if ver.iter().all(|b| b.is_ascii_graphic()) {
        let v = String::from_utf8_lossy(ver);
        format!("{name}/{v}")
    } else {
        name.to_string()
    }
}

fn mainline_style(id: &[u8; 20]) -> Option<String> {
    // Mmajor-minor-patch-- + rest  (e.g. M4-20-8--… or M6-0-0--)
    if id[0] != b'M' {
        return None;
    }
    let s = std::str::from_utf8(id).ok()?;
    let end = s.find("--").unwrap_or(s.len().min(12));
    let ver = s.get(1..end)?.trim_matches('-');
    if ver.is_empty() || !ver.bytes().all(|b| b.is_ascii_digit() || b == b'-') {
        return None;
    }
    Some(format!("Mainline {ver}"))
}

fn shadow_style(id: &[u8; 20]) -> Option<String> {
    // First char = client, next digits = version (up to 5), then '-'
    let client = match id[0] {
        b'A' => "ABC",
        b'O' => "Osprey",
        b'Q' => "BTQueue",
        b'R' => "Tribler",
        b'S' => "Shadow",
        b'T' => "BitTornado",
        b'U' => "UPnP NAT Bit Torrent",
        _ => return None,
    };
    let mut ver = String::new();
    for &b in &id[1..] {
        if b.is_ascii_digit() {
            ver.push(b as char);
        } else {
            break;
        }
    }
    if ver.is_empty() {
        Some(client.into())
    } else {
        Some(format!("{client} {ver}"))
    }
}

fn printable_fallback(id: &[u8; 20]) -> String {
    let mut s = String::new();
    for &b in id.iter().take(12) {
        if b.is_ascii_graphic() && b != b' ' {
            s.push(b as char);
        } else {
            break;
        }
    }
    if s.len() >= 2 {
        format!("peer:{s}")
    } else {
        "unknown".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pid(prefix: &str) -> [u8; 20] {
        let mut id = [0u8; 20];
        let p = prefix.as_bytes();
        let n = p.len().min(20);
        id[..n].copy_from_slice(&p[..n]);
        // pad rest with printable
        for b in &mut id[n..] {
            *b = b'x';
        }
        id
    }

    #[test]
    fn azureus_qbittorrent() {
        let s = identify_peer_id(&pid("-qB4600-abcdefgh"));
        assert!(s.starts_with("qBittorrent"), "{s}");
        assert!(s.contains("4.6"), "{s}");
    }

    #[test]
    fn azureus_libtorrent_lt() {
        let s = identify_peer_id(&pid("-lt0F07-xxxxxxxxxxxx"));
        assert!(s.starts_with("libtorrent"), "{s}");
    }

    #[test]
    fn azureus_transmission() {
        let s = identify_peer_id(&pid("-TR4060-xxxxxxxxxxxx"));
        assert!(s.starts_with("Transmission"), "{s}");
    }

    #[test]
    fn azureus_seedchamp() {
        let s = identify_peer_id(&pid("-sc0001-xxxxxxxxxxxx"));
        assert!(s.starts_with("seedchamp"), "{s}");
    }

    #[test]
    fn mainline() {
        let s = identify_peer_id(&pid("M6-0-0--xxxxxxxxxxxx"));
        assert!(s.starts_with("Mainline"), "{s}");
    }

    #[test]
    fn unknown_random() {
        let s = identify_peer_id(&[0u8; 20]);
        assert_eq!(s, "unknown");
    }

    #[test]
    fn ltep_v_prefers_string() {
        // d1:v13:qBittorrent/4e
        let payload = b"d1:v13:qBittorrent/4e";
        assert_eq!(
            ltep_client_version(payload).as_deref(),
            Some("qBittorrent/4")
        );
        assert_eq!(
            prefer_client_label("libtorrent 0.15", Some("qBittorrent/4.6.0")),
            "qBittorrent/4.6.0"
        );
    }
}
