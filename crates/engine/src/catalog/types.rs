//! Catalog DTOs.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::metainfo::Metainfo;

/// Input for inserting a torrent into the catalog.
#[derive(Debug, Clone)]
pub struct TorrentInsert {
    pub metainfo: Metainfo,
    pub state: String,
    pub want_start: bool,
    pub complete: bool,
    pub bitfield: Option<Vec<u8>>,
    pub have_count: u32,
    pub data_root: String,
    /// Permanent library root when staged on leech_cache (`None` / empty = not staged).
    pub home_root: Option<String>,
    pub source_torrent: Option<String>,
    pub uploaded: u64,
    pub downloaded: u64,
    pub finished_at: Option<i64>,
    /// Unix time when the torrent was first added (rtorrent `timestamp.started`, etc.).
    /// `None` → insert uses wall clock "now".
    pub created_at: Option<i64>,
    pub file_priorities: Vec<i32>,
    /// Exact original `.torrent` file bytes (for perfect re-export / same infohash).
    pub metainfo_blob: Option<Vec<u8>>,
    /// rtorrent-style announce `key` (uint32, never 0). `None` → generate on insert.
    pub tracker_key: Option<u32>,
}

impl TorrentInsert {
    pub fn from_metainfo(metainfo: Metainfo, data_root: impl Into<String>) -> Self {
        let complete = false;
        let have_count = 0;
        Self {
            metainfo,
            state: "stopped".into(),
            want_start: false,
            complete,
            bitfield: None,
            have_count,
            data_root: data_root.into(),
            home_root: None,
            source_torrent: None,
            uploaded: 0,
            downloaded: 0,
            finished_at: None,
            created_at: None,
            file_priorities: Vec::new(),
            metainfo_blob: None,
            tracker_key: None,
        }
    }

    pub fn now_unix() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }
}

/// Row for TUI / CLI list views.
#[derive(Debug, Clone)]
pub struct TorrentListRow {
    pub id: i64,
    pub infohash_hex: String,
    pub name: String,
    pub total_size: u64,
    pub piece_count: u32,
    pub state: String,
    pub complete: bool,
    pub want_start: bool,
    pub uploaded: u64,
    pub downloaded: u64,
    pub data_root: Option<String>,
    pub have_count: u32,
    /// Unix time when the torrent was added to the catalog.
    pub created_at: i64,
    /// Catalog error (e.g. startup storage demote); shown as RUN `err` when stopped.
    pub error_msg: Option<String>,
}

/// File row for detail view.
#[derive(Debug, Clone)]
pub struct FileRow {
    pub idx: u32,
    pub path: String,
    pub size: u64,
    pub offset: u64,
    /// `0` = do not download; `≥1` = download (normal). Priority levels reserved.
    pub priority: i32,
}

impl FileRow {
    pub fn wanted(&self) -> bool {
        self.priority > 0
    }
}

/// File row plus how many of its bytes are present (from piece bitfield).
#[derive(Debug, Clone)]
pub struct FileProgress {
    pub file: FileRow,
    pub have_bytes: u64,
}

impl FileProgress {
    pub fn wanted(&self) -> bool {
        self.file.wanted()
    }

    /// 0–100; 100 when fully present (or empty file).
    pub fn pct(&self) -> u32 {
        if self.file.size == 0 {
            return 100;
        }
        if self.have_bytes >= self.file.size {
            return 100;
        }
        ((100.0 * self.have_bytes as f64 / self.file.size as f64).floor() as u32).min(99)
    }

    pub fn done(&self) -> bool {
        self.have_bytes >= self.file.size
    }
}

/// Tracker row for detail view (includes last announce stats when known).
#[derive(Debug, Clone)]
pub struct TrackerRow {
    pub id: i64,
    pub url: String,
    pub tier: u32,
    pub enabled: bool,
    /// Tracker-reported seeders (`complete`); `None` if never announced / omitted.
    pub seeders: Option<u32>,
    /// Tracker-reported leechers (`incomplete`).
    pub leechers: Option<u32>,
    /// Unix seconds of last announce attempt that wrote status.
    pub last_announce_at: Option<i64>,
    /// Last successful interval (seconds).
    pub last_interval: Option<u32>,
    /// Peers returned on last successful announce.
    pub last_peers: Option<u32>,
    /// `"ok"` or truncated failure/error text.
    pub last_status: Option<String>,
}

/// Full torrent detail for TUI.
#[derive(Debug, Clone)]
pub struct TorrentDetail {
    pub list: TorrentListRow,
    pub piece_length: u32,
    pub private: bool,
    pub error_msg: Option<String>,
    pub source_torrent: Option<String>,
    pub corrupted: u64,
    pub finished_at: Option<i64>,
    pub files: Vec<FileRow>,
    pub trackers: Vec<TrackerRow>,
}

impl TorrentDetail {
    /// Best-effort swarm S/L from the most recent successful tracker row.
    pub fn swarm_sl(&self) -> (Option<u32>, Option<u32>) {
        let mut best: Option<&TrackerRow> = None;
        for t in &self.trackers {
            if t.seeders.is_none() && t.leechers.is_none() {
                continue;
            }
            match best {
                None => best = Some(t),
                Some(b) => {
                    let ta = t.last_announce_at.unwrap_or(0);
                    let ba = b.last_announce_at.unwrap_or(0);
                    if ta >= ba {
                        best = Some(t);
                    }
                }
            }
        }
        best.map(|t| (t.seeders, t.leechers))
            .unwrap_or((None, None))
    }
}

/// Global session limits (catalog `setting` table).
#[derive(Debug, Clone)]
pub struct SessionLimits {
    /// Max upload bytes/sec; 0 = unlimited.
    pub max_upload_bps: u64,
    /// Max download bytes/sec; 0 = unlimited.
    pub max_download_bps: u64,
    /// Useful-peer floor (leech chase; seed only if seed_dial_peers).
    pub min_peers: u32,
    /// Max concurrent peers **per torrent** (inbound + outbound).
    pub max_peers: u32,
}

impl Default for SessionLimits {
    fn default() -> Self {
        Self {
            max_upload_bps: 0,
            max_download_bps: 0,
            min_peers: 20,
            max_peers: 40,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct TorrentStats {
    pub uploaded: u64,
    pub downloaded: u64,
    pub corrupted: u64,
    pub active_time: u64,
    pub finished_at: Option<i64>,
}

/// Bitfield helpers.
pub fn bitfield_size_bytes(piece_count: u32) -> usize {
    (piece_count as usize).div_ceil(8)
}

pub fn count_have_bits(bits: &[u8], piece_count: u32) -> u32 {
    let mut n = 0u32;
    for i in 0..piece_count as usize {
        let byte = bits.get(i / 8).copied().unwrap_or(0);
        if byte & (1 << (7 - (i % 8))) != 0 {
            n += 1;
        }
    }
    n
}

pub fn all_set_bitfield(piece_count: u32) -> Vec<u8> {
    let mut bits = vec![0xffu8; bitfield_size_bytes(piece_count)];
    if piece_count > 0 {
        let rem = (piece_count % 8) as u8;
        if rem != 0 {
            let last = bits.len() - 1;
            bits[last] = 0xffu8 << (8 - rem);
        }
    }
    bits
}

pub fn empty_bitfield(piece_count: u32) -> Vec<u8> {
    vec![0u8; bitfield_size_bytes(piece_count)]
}

pub fn bitfield_get(bits: &[u8], index: u32) -> bool {
    let i = index as usize;
    let byte = bits.get(i / 8).copied().unwrap_or(0);
    byte & (1 << (7 - (i % 8))) != 0
}

pub fn bitfield_set(bits: &mut [u8], index: u32) {
    let i = index as usize;
    if i / 8 < bits.len() {
        bits[i / 8] |= 1 << (7 - (i % 8));
    }
}
