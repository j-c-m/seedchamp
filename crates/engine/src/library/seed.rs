//! Peer-id helpers and background seed-loop handle (CLI/library).

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use rand::Rng;

use crate::error::{Error, Result};

use crate::library::run::serve_main;
use crate::session::RuntimeConfig;

pub struct SeedHandle {
    stop: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
}

impl SeedHandle {
    pub fn stop(self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(j) = self.join {
            let _ = j.join();
        }
    }
}

/// Full package version (`1.0.0`), no git sha — CLI `version` / doctor engine line.
pub const PKG_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Major component of [`PKG_VERSION`] only (`"1"` for `1.0.0`).
///
/// Wire identity (LTEP `v`, HTTP User-Agent) uses major alone so minor/patch
/// bumps do not churn tracker/client fingerprints.
pub fn pkg_version_major() -> &'static str {
    match PKG_VERSION.split_once('.') {
        Some((m, _)) if !m.is_empty() => m,
        _ => PKG_VERSION,
    }
}

/// Fixed Azureus-style peer-id prefix for seedchamp: `-sc0001-`.
///
/// Brand code `sc` is unused in common peer-id maps. Keep fixed so trackers
/// see a stable identity; version rides on LTEP `v` and User-Agent (major only).
///
/// Override via `network.peer_id_prefix` / `SEEDCHAMP_PEER_ID_PREFIX` /
/// `SEEDCHAMP_IDENTITY` (alias or raw Azureus prefix).
pub const DEFAULT_PEER_ID_PREFIX: &[u8] = b"-sc0001-";

/// BEP 10 LTEP extended-handshake `v` for the default identity.
///
/// Major only, e.g. `seedchamp 1`. Full build id remains on the CLI `version`
/// command via [`crate::VERSION`].
pub fn default_ltep_client() -> String {
    format!("seedchamp {}", pkg_version_major())
}

/// Resolve LTEP extended-handshake `v` from peer-id identity config.
///
/// Always seedchamp package version unless `network.ltep_client` is set
/// explicitly (non-empty) when building `RuntimeConfig`.
pub fn resolve_ltep_client(_s: &str) -> String {
    default_ltep_client()
}

/// Generate Azureus-style peer id: `prefix` (up to 20 bytes) + random pad to 20.
pub fn generate_peer_id_with_prefix(prefix: &[u8]) -> [u8; 20] {
    let mut id = [0u8; 20];
    let n = prefix.len().min(20);
    if n > 0 {
        id[..n].copy_from_slice(&prefix[..n]);
    }
    if n < 20 {
        rand::rng().fill_bytes(&mut id[n..]);
    }
    id
}

/// Generate Azureus-style peer id (default `-sc0001-` + 12 random bytes).
pub fn generate_peer_id() -> [u8; 20] {
    generate_peer_id_with_prefix(DEFAULT_PEER_ID_PREFIX)
}

/// Resolve a config identity / prefix string to peer-id prefix bytes.
///
/// Accepts:
/// - aliases: empty / `default` / `seedchamp` / `sc` → [`DEFAULT_PEER_ID_PREFIX`]
/// - raw Azureus prefix, e.g. `-sc0001-` (case-sensitive bytes kept as given)
pub fn resolve_peer_id_prefix(s: &str) -> Vec<u8> {
    let t = s.trim();
    match t.to_ascii_lowercase().as_str() {
        "" | "default" | "seedchamp" | "sc" => DEFAULT_PEER_ID_PREFIX.to_vec(),
        // Raw prefix (keep original case — peer ids are case-sensitive bytes).
        _ => t.as_bytes().to_vec(),
    }
}

#[cfg(test)]
mod peer_id_tests {
    use super::*;

    #[test]
    fn default_prefix_fixed_sc0001() {
        assert_eq!(DEFAULT_PEER_ID_PREFIX, b"-sc0001-");
        let id = generate_peer_id();
        assert_eq!(&id[..8], b"-sc0001-");
        assert_eq!(id.len(), 20);
    }

    #[test]
    fn seedchamp_aliases() {
        assert_eq!(resolve_peer_id_prefix(""), b"-sc0001-");
        assert_eq!(resolve_peer_id_prefix("default"), b"-sc0001-");
        assert_eq!(resolve_peer_id_prefix("seedchamp"), b"-sc0001-");
        assert_eq!(resolve_peer_id_prefix("sc"), b"-sc0001-");
    }

    #[test]
    fn raw_prefix_passthrough() {
        assert_eq!(resolve_peer_id_prefix("-sc0001-"), b"-sc0001-");
        assert_eq!(resolve_peer_id_prefix("-XX9999-"), b"-XX9999-");
    }

    #[test]
    fn ltep_v_default_is_seedchamp_major_only() {
        let d = resolve_ltep_client("");
        assert_eq!(d, format!("seedchamp {}", pkg_version_major()));
        assert_eq!(d, "seedchamp 1");
        assert_eq!(resolve_ltep_client("seedchamp"), d);
        assert_eq!(resolve_ltep_client("-sc0001-"), d);
        assert_eq!(resolve_ltep_client("anything"), d);
        assert!(!d.contains('.'));
        assert!(!d.contains(env!("GIT_SHA")));
    }

    #[test]
    fn pkg_version_major_strips_minor_patch() {
        assert_eq!(pkg_version_major(), "1");
        assert!(PKG_VERSION.starts_with("1."));
    }
}

/// Run headless serve loop in a background thread. Returns handle to stop.
///
/// Uses catalog `want_start` only (no force-start list). Runs until handle stop.
pub fn run_serve_loop(db_path: &Path, rt: RuntimeConfig) -> Result<SeedHandle> {
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let db = db_path.to_path_buf();
    let join = thread::Builder::new()
        .name("seedchamp-serve".into())
        .spawn(move || {
            if let Err(e) = serve_main(&db, rt, Vec::new(), false, stop2) {
                tracing::error!(error = %e, "serve loop exited");
                eprintln!("seedchamp serve error: {e}");
            }
        })
        .map_err(|e| Error::Msg(format!("spawn serve: {e}")))?;
    Ok(SeedHandle {
        stop,
        join: Some(join),
    })
}
