//! Parse `.rtorrent` sidecar bencode.

use seedchamp_engine::bencode;
use seedchamp_engine::{Error, Result};

#[derive(Debug, Default)]
pub struct RtorrentSide {
    pub directory: Option<String>,
    pub directory_base: Option<String>,
    /// First start / activity (unix seconds). 0 / missing = unknown.
    pub timestamp_started: Option<i64>,
    /// Finished complete (unix seconds).
    pub timestamp_finished: Option<i64>,
    /// Last state change (unix seconds); fallback when started is 0.
    pub state_changed: Option<i64>,
    /// Lifetime totals stored in the `.rtorrent` map (often more complete than resume).
    pub total_uploaded: Option<u64>,
    pub total_downloaded: Option<u64>,
    /// rtorrent announce key (`rtorrent.key`, uint32).
    pub key: Option<u32>,
}

impl RtorrentSide {
    /// Preferred data root for files.
    pub fn data_root(&self) -> Option<String> {
        self.directory
            .clone()
            .or_else(|| self.directory_base.clone())
    }

    /// Best estimate of "when was this torrent first known to rtorrent".
    ///
    /// Order: `timestamp.started` → `state_changed` → `timestamp.finished` (all > 0).
    pub fn created_at_hint(&self) -> Option<i64> {
        nonzero(self.timestamp_started)
            .or_else(|| nonzero(self.state_changed))
            .or_else(|| nonzero(self.timestamp_finished))
    }

    pub fn finished_at_hint(&self) -> Option<i64> {
        nonzero(self.timestamp_finished)
    }
}

fn nonzero(v: Option<i64>) -> Option<i64> {
    v.filter(|&t| t > 0)
}

fn dict_u64(root: &seedchamp_engine::bencode::Value, key: &str) -> Option<u64> {
    root.dict_get_int(key).map(|n| n.max(0) as u64)
}

pub fn parse_rtorrent(bytes: &[u8]) -> Result<RtorrentSide> {
    let root =
        bencode::decode_full(bytes).map_err(|e| Error::Msg(format!("rtorrent side: {e}")))?;
    let mut out = RtorrentSide::default();
    if let Some(s) = root.dict_get_str("directory") {
        out.directory = Some(s.to_string());
    }
    if let Some(s) = root.dict_get_str("directory.base") {
        out.directory_base = Some(s.to_string());
    }
    // Some versions use directory_base
    if out.directory_base.is_none() {
        if let Some(s) = root.dict_get_str("directory_base") {
            out.directory_base = Some(s.to_string());
        }
    }
    out.timestamp_started = root.dict_get_int("timestamp.started");
    out.timestamp_finished = root.dict_get_int("timestamp.finished");
    out.state_changed = root.dict_get_int("state_changed");
    out.total_uploaded = dict_u64(&root, "total_uploaded");
    out.total_downloaded = dict_u64(&root, "total_downloaded");
    // rtorrent writes `key` as an integer (never 0).
    if let Some(n) = root.dict_get_int("key") {
        if n > 0 && n <= u32::MAX as i64 {
            out.key = Some(n as u32);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_timestamps_and_totals() {
        // Minimal map with the keys rtorrent writes to HASH.torrent.rtorrent
        let raw = b"d9:directory3:/dl17:timestamp.startedi1700000000e18:timestamp.finishedi1700000100e13:state_changedi1700000001e14:total_uploadedi999e16:total_downloadedi42e3:keyi305419896ee";
        let s = parse_rtorrent(raw).unwrap();
        assert_eq!(s.directory.as_deref(), Some("/dl"));
        assert_eq!(s.timestamp_started, Some(1_700_000_000));
        assert_eq!(s.timestamp_finished, Some(1_700_000_100));
        assert_eq!(s.state_changed, Some(1_700_000_001));
        assert_eq!(s.total_uploaded, Some(999));
        assert_eq!(s.total_downloaded, Some(42));
        assert_eq!(s.key, Some(0x1234_5678));
        assert_eq!(s.created_at_hint(), Some(1_700_000_000));
        assert_eq!(s.finished_at_hint(), Some(1_700_000_100));
    }

    #[test]
    fn zero_timestamps_ignored() {
        let raw = b"d17:timestamp.startedi0e18:timestamp.finishedi0ee";
        let s = parse_rtorrent(raw).unwrap();
        assert_eq!(s.created_at_hint(), None);
        assert_eq!(s.finished_at_hint(), None);
    }
}
