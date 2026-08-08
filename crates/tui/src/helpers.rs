//! Small TUI helpers (status parsing, default data root, bps parse).

use std::path::{Path, PathBuf};

pub(crate) fn default_data_root(db: &Path) -> PathBuf {
    db.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.join("downloads"))
        .unwrap_or_else(|| PathBuf::from("downloads"))
}

pub(crate) fn status_msg_is_error(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    // Normal recheck reports include "missing=N" — not sticky errors.
    if m.contains("recheck id=") {
        return false;
    }
    m.contains("fail")
        || m.contains("error")
        || m.contains("busy")
        || m.contains("catalog missing")
        || m.contains("denied")
        || m.contains("refused")
        || m.contains("timeout")
        || m.contains("no space")
        || m.starts_with("bind ")
        || is_disk_worker_dead_status(msg)
}

/// Permanent disk-worker death — TUI sticky override until process restart.
pub(crate) fn is_disk_worker_dead_status(msg: &str) -> bool {
    msg.contains(seedchamp_engine::DISK_WORKER_DEAD_STATUS)
        || msg.to_ascii_lowercase().contains("disk worker dead")
}

pub(crate) fn status_msg_clears_error(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    // Never clear sticky disk death with a casual lifecycle event.
    if is_disk_worker_dead_status(msg) {
        return false;
    }
    // Successful lifecycle — clear sticky error so the bar can go blank.
    (m.contains("started") && !m.contains("fail"))
        || (m.contains("stopped") && !m.contains("fail"))
        || m.contains("recheck id=")
        || m.contains("recheck ok")
        || m.contains("control ready")
        || m.contains("control up")
}

pub(crate) fn parse_bps(s: &str) -> u64 {
    let s = s.trim().to_ascii_lowercase();
    if s == "0" || s == "inf" || s == "unlimited" {
        return 0;
    }
    let (num, mult) = if let Some(n) = s.strip_suffix('k') {
        (n, 1000u64)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 1_000_000)
    } else {
        (s.as_str(), 1)
    };
    num.parse::<f64>()
        .ok()
        .map(|v| (v * mult as f64) as u64)
        .unwrap_or(0)
}
