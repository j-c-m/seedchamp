//! Control plane command, event, and mutation job types.

use std::path::PathBuf;

use crate::catalog::{SessionLimits, TorrentListRow};

pub enum EngineCommand {
    StartTorrent {
        id: i64,
    },
    StopTorrent {
        id: i64,
    },
    Recheck {
        id: i64,
    },
    /// `priority` 0 = off, ≥1 = on.
    SetFilePriority {
        torrent_id: i64,
        file_idx: u32,
        priority: i32,
    },
    /// Live (or cold) data_root relocate — publish dest then swap; no stop/start.
    Relocate {
        id: i64,
        new_root: PathBuf,
    },
    /// Soft-delete (hide from lists; payload kept). Stop if hot first.
    SoftDelete {
        id: i64,
    },
    /// Hard-remove catalog rows (CASCADE); payload kept. Stop if hot first.
    Remove {
        id: i64,
    },
    /// Catalog + live wire/peer session limits.
    SetSessionLimits {
        limits: SessionLimits,
    },
    /// Full TUI list (`list_torrents_filtered` + session limits) on the catalog **reader**.
    ListCatalog {
        filter: String,
    },
    Shutdown,
}

/// Async replies / status from control → TUI (non-blocking poll).
#[derive(Debug, Clone)]
pub enum ControlEvent {
    /// Free-form status line (announce progress, etc.).
    Status(String),
    Started {
        id: i64,
    },
    StartFailed {
        id: i64,
        error: String,
    },
    Stopped {
        id: i64,
    },
    StopFailed {
        id: i64,
        error: String,
    },
    /// Live recheck progress (HAVE can count 0 → good).
    RecheckProgress {
        id: i64,
        piece_count: u32,
        checked: u32,
        good: u32,
        bad: u32,
        missing: u32,
    },
    Rechecked {
        id: i64,
        message: String,
        complete: bool,
        good: u32,
        bad: u32,
        missing: u32,
        piece_count: u32,
    },
    RecheckFailed {
        id: i64,
        error: String,
    },
    Relocated {
        id: i64,
        data_root: PathBuf,
        note: String,
    },
    RelocateFailed {
        id: i64,
        error: String,
    },
    SoftDeleted {
        id: i64,
    },
    SoftDeleteFailed {
        id: i64,
        error: String,
    },
    Removed {
        id: i64,
    },
    RemoveFailed {
        id: i64,
        error: String,
    },
    LimitsUpdated {
        limits: SessionLimits,
    },
    LimitsFailed {
        error: String,
    },
    /// Full catalog list for TUI (catalog reader thread).
    CatalogList {
        filter: String,
        rows: Vec<TorrentListRow>,
        limits: SessionLimits,
    },
    CatalogListFailed {
        filter: String,
        error: String,
    },
    Ready {
        listen: String,
        peer_workers: usize,
    },
}

/// Jobs for the serial **mutation** worker only (no RO list scans).
pub(super) enum MutationJob {
    Start(i64),
    Stop(i64),
    Recheck(i64),
    /// After detached recheck: stop (if hot) then optionally start so RAM bitfield matches catalog.
    SyncAfterRecheck {
        id: i64,
        start: bool,
    },
    SetFilePriority {
        torrent_id: i64,
        file_idx: u32,
        priority: i32,
    },
    Relocate {
        id: i64,
        new_root: PathBuf,
    },
    SoftDelete(i64),
    Remove(i64),
    SetSessionLimits(SessionLimits),
    Shutdown,
}

/// Jobs for the catalog **read-only** worker (`seedchamp-cread`).
pub(super) enum CatalogReadJob {
    ListCatalog { filter: String },
    Shutdown,
}
