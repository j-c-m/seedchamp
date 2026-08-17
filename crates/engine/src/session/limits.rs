//! Live session limits (wire rate + peer floor/ceil).

use std::sync::atomic::Ordering;

use crate::catalog::SessionLimits;
use crate::error::Result;

impl super::SessionRuntime {
    /// Catalog + live session limits (wire caps and peer floor/ceil).
    ///
    /// Control plane only (mutation worker). `0` wire caps = unlimited.
    pub fn apply_session_limits(&self, lim: &SessionLimits) -> Result<()> {
        let max_p = lim.max_peers.max(1) as usize;
        let min_p = (lim.min_peers as usize).min(max_p).max(1);
        self.with_catalog_mut(|cat| cat.set_session_limits(lim))?;
        self.inner
            .wire_limiter
            .set_caps(lim.max_upload_bps, lim.max_download_bps);
        self.inner.max_peers.store(max_p, Ordering::Relaxed);
        self.inner.min_peers.store(min_p, Ordering::Relaxed);
        Ok(())
    }
}
