//! Session settings and limits KV.
//!
//! Methods on [`Catalog`]. Column `want_start` means the user wants the torrent
//! **active in the swarm**.

use rusqlite::{params, OptionalExtension};

use super::open::Catalog;
use super::types::SessionLimits;
use crate::error::Result;

impl Catalog {
    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        let v = self
            .conn
            .query_row(
                "SELECT value FROM setting WHERE key = ?1",
                params![key],
                |r| r.get(0),
            )
            .optional()?;
        Ok(v)
    }

    pub fn set_setting(&mut self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO setting (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn session_limits(&self) -> Result<SessionLimits> {
        let mut lim = SessionLimits::default();
        if let Some(v) = self.get_setting("max_upload_bps")? {
            lim.max_upload_bps = v.parse().unwrap_or(0);
        }
        if let Some(v) = self.get_setting("max_download_bps")? {
            lim.max_download_bps = v.parse().unwrap_or(0);
        }
        if let Some(v) = self.get_setting("min_peers")? {
            lim.min_peers = v.parse().unwrap_or(20);
        }
        if let Some(v) = self.get_setting("max_peers")? {
            lim.max_peers = v.parse().unwrap_or(40);
        }
        let max_peers = lim.max_peers.max(1);
        lim.max_peers = max_peers;
        lim.min_peers = lim.min_peers.min(max_peers);
        Ok(lim)
    }

    pub fn set_session_limits(&mut self, lim: &SessionLimits) -> Result<()> {
        let max_peers = lim.max_peers.max(1);
        let min_peers = lim.min_peers.min(max_peers);
        self.set_setting("max_upload_bps", &lim.max_upload_bps.to_string())?;
        self.set_setting("max_download_bps", &lim.max_download_bps.to_string())?;
        self.set_setting("min_peers", &min_peers.to_string())?;
        self.set_setting("max_peers", &max_peers.to_string())?;
        Ok(())
    }
}
