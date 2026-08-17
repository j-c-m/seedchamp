//! Open / migrate catalog database.

use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension};

use crate::error::{Error, Result};

const SCHEMA_SQL: &str = include_str!("schema.sql");
const SCHEMA_VERSION: i64 = 8;

/// SQLite catalog (session authority).
pub struct Catalog {
    pub(crate) conn: Connection,
    path: Option<std::path::PathBuf>,
}

impl Catalog {
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with_busy_timeout(path, Duration::from_secs(5))
    }

    /// Open catalog with a custom SQLite busy timeout.
    ///
    /// Engine paths use a multi-second timeout. **TUI** should use a short
    /// timeout (tens of ms) so list refresh never freezes the UI behind
    /// `mark_pieces_have_batch` / other writers during fast download.
    pub fn open_with_busy_timeout(path: &Path, busy: Duration) -> Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let conn = Connection::open(path)?;
        // Avoid indefinite hangs when UI + control + workers open the same DB.
        conn.busy_timeout(busy)?;
        let mut cat = Self {
            conn,
            path: Some(path.to_path_buf()),
        };
        cat.migrate()?;
        cat.apply_runtime_pragmas()?;
        Ok(cat)
    }

    /// TUI / interactive readers: fail fast when the engine is mid-write.
    pub fn open_for_ui(path: &Path) -> Result<Self> {
        Self::open_with_busy_timeout(path, Duration::from_millis(50))
    }

    /// Lower busy timeout on an already-open connection (e.g. after engine open).
    pub fn set_busy_timeout(&self, busy: Duration) -> Result<()> {
        self.conn.busy_timeout(busy)?;
        Ok(())
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.busy_timeout(Duration::from_secs(5))?;
        let mut cat = Self { conn, path: None };
        cat.migrate()?;
        cat.apply_runtime_pragmas()?;
        Ok(cat)
    }

    /// Per-connection tunables (cheap; re-applied on every open).
    ///
    /// - WAL: concurrent readers (TUI) + writer (engine)
    /// - synchronous=NORMAL: good durability with WAL, less fsync than FULL
    /// - cache_size=-65536: ~64 MiB page cache (helps bitfield blob RMW)
    fn apply_runtime_pragmas(&self) -> Result<()> {
        let _ = self.conn.pragma_update(None, "journal_mode", "WAL");
        self.conn.pragma_update(None, "synchronous", "NORMAL")?;
        // Negative cache_size = KiB units in SQLite.
        self.conn.pragma_update(None, "cache_size", -65536)?;
        Ok(())
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub fn conn_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }

    fn migrate(&mut self) -> Result<()> {
        // Always cheap.
        self.conn.pragma_update(None, "foreign_keys", true)?;

        // Fast path when schema_version exists: migrate stepwise, never re-run
        // full SCHEMA_SQL (CREATE IF NOT EXISTS) on every open.
        match self
            .conn
            .query_row("SELECT version FROM schema_version LIMIT 1", [], |r| {
                r.get::<_, i64>(0)
            }) {
            Ok(v) if v == SCHEMA_VERSION => return Ok(()),
            Ok(v) => {
                self.upgrade_from(v)?;
                return Ok(());
            }
            Err(_) => {
                // Missing table or empty DB — full bootstrap below.
            }
        }

        // First open / fresh DB: WAL + full schema.
        let _ = self.conn.pragma_update(None, "journal_mode", "WAL");
        self.conn.execute_batch(SCHEMA_SQL)?;

        let ver: Option<i64> = self
            .conn
            .query_row("SELECT version FROM schema_version LIMIT 1", [], |r| {
                r.get(0)
            })
            .optional()?;

        match ver {
            None => {
                self.conn.execute(
                    "INSERT INTO schema_version (version) VALUES (?1)",
                    [SCHEMA_VERSION],
                )?;
            }
            Some(v) if v == SCHEMA_VERSION => {}
            // schema_version row existed but version lag (shouldn't hit after
            // execute_batch IF NOT EXISTS on existing DB — handle anyway).
            Some(v) => self.upgrade_from(v)?,
        }
        Ok(())
    }

    /// Stepwise schema upgrades from `from` (inclusive of next step) to [`SCHEMA_VERSION`].
    fn upgrade_from(&mut self, from: i64) -> Result<()> {
        if from == SCHEMA_VERSION {
            return Ok(());
        }
        if !(1..=SCHEMA_VERSION).contains(&from) {
            return Err(Error::Msg(format!(
                "unsupported schema version {from} (want {SCHEMA_VERSION})"
            )));
        }
        let mut v = from;
        while v < SCHEMA_VERSION {
            match v {
                1 => self.migrate_v1_to_v2()?,
                2 => self.migrate_v2_to_v3()?,
                3 => self.migrate_v3_to_v4()?,
                4 => self.migrate_v4_to_v5()?,
                5 => self.migrate_v5_to_v6()?,
                6 => self.migrate_v6_to_v7()?,
                7 => self.migrate_v7_to_v8()?,
                other => {
                    return Err(Error::Msg(format!(
                        "unsupported schema version {other} (want {SCHEMA_VERSION})"
                    )));
                }
            }
            v += 1;
        }
        Ok(())
    }

    fn migrate_v1_to_v2(&mut self) -> Result<()> {
        // Soft-delete column for Ctrl-D (payload left on disk).
        let has_deleted: bool = self
            .conn
            .prepare("PRAGMA table_info(torrent)")?
            .query_map([], |r| r.get::<_, String>(1))?
            .filter_map(|c| c.ok())
            .any(|name| name == "deleted");
        if !has_deleted {
            self.conn.execute(
                "ALTER TABLE torrent ADD COLUMN deleted INTEGER NOT NULL DEFAULT 0",
                [],
            )?;
        }
        self.conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_torrent_deleted ON torrent(deleted)",
            [],
        )?;
        self.conn
            .execute("UPDATE schema_version SET version = 2", [])?;
        Ok(())
    }

    fn migrate_v2_to_v3(&mut self) -> Result<()> {
        // Exact original .torrent bytes for perfect export.
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS torrent_metainfo (
               torrent_id INTEGER PRIMARY KEY REFERENCES torrent(id) ON DELETE CASCADE,
               blob       BLOB NOT NULL
             );",
        )?;
        self.conn
            .execute("UPDATE schema_version SET version = 3", [])?;
        Ok(())
    }

    fn migrate_v3_to_v4(&mut self) -> Result<()> {
        // rtorrent-compatible announce key (stable per torrent).
        let has: bool = self
            .conn
            .prepare("PRAGMA table_info(torrent)")?
            .query_map([], |r| r.get::<_, String>(1))?
            .filter_map(|c| c.ok())
            .any(|name| name == "tracker_key");
        if !has {
            self.conn.execute(
                "ALTER TABLE torrent ADD COLUMN tracker_key INTEGER NOT NULL DEFAULT 0",
                [],
            )?;
        }
        self.conn
            .execute("UPDATE schema_version SET version = 4", [])?;
        Ok(())
    }

    fn migrate_v4_to_v5(&mut self) -> Result<()> {
        // Soft-delete timestamp for startup catalog purge (payload still never deleted).
        let has: bool = self
            .conn
            .prepare("PRAGMA table_info(torrent)")?
            .query_map([], |r| r.get::<_, String>(1))?
            .filter_map(|c| c.ok())
            .any(|name| name == "deleted_at");
        if !has {
            self.conn
                .execute("ALTER TABLE torrent ADD COLUMN deleted_at INTEGER", [])?;
        }
        // Already soft-deleted rows: stamp "now" so they get a full retention window
        // rather than being purged immediately on upgrade.
        self.conn.execute(
            "UPDATE torrent SET deleted_at = CAST(strftime('%s','now') AS INTEGER)
             WHERE COALESCE(deleted, 0) != 0 AND deleted_at IS NULL",
            [],
        )?;
        self.conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_torrent_deleted_at ON torrent(deleted_at)",
            [],
        )?;
        self.conn
            .execute("UPDATE schema_version SET version = 5", [])?;
        Ok(())
    }

    fn migrate_v5_to_v6(&mut self) -> Result<()> {
        // Per-tracker announce stats for TUI detail (S/L, last status).
        let cols: Vec<String> = self
            .conn
            .prepare("PRAGMA table_info(tracker)")?
            .query_map([], |r| r.get::<_, String>(1))?
            .filter_map(|c| c.ok())
            .collect();
        let add = |conn: &Connection, name: &str, decl: &str| -> Result<()> {
            if !cols.iter().any(|c| c == name) {
                conn.execute(&format!("ALTER TABLE tracker ADD COLUMN {name} {decl}"), [])?;
            }
            Ok(())
        };
        add(&self.conn, "seeders", "INTEGER")?;
        add(&self.conn, "leechers", "INTEGER")?;
        add(&self.conn, "last_announce_at", "INTEGER")?;
        add(&self.conn, "last_interval", "INTEGER")?;
        add(&self.conn, "last_peers", "INTEGER")?;
        add(&self.conn, "last_status", "TEXT")?;
        self.conn
            .execute("UPDATE schema_version SET version = 6", [])?;
        Ok(())
    }

    fn migrate_v6_to_v7(&mut self) -> Result<()> {
        // Indexes for hot catalog paths (peer_cache list/prune, tracker announce, want_start).
        self.conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_torrent_want_start_deleted ON torrent(want_start, deleted);
             CREATE INDEX IF NOT EXISTS idx_peer_cache_torrent_seen ON peer_cache(torrent_id, last_seen DESC);
             CREATE INDEX IF NOT EXISTS idx_tracker_torrent_url ON tracker(torrent_id, url);",
        )?;
        self.conn
            .execute("UPDATE schema_version SET version = 7", [])?;
        Ok(())
    }

    fn migrate_v7_to_v8(&mut self) -> Result<()> {
        // Permanent library root when payload is staged under paths.leech_cache.
        let cols: Vec<String> = self
            .conn
            .prepare("PRAGMA table_info(meta_path)")?
            .query_map([], |r| r.get::<_, String>(1))?
            .filter_map(|c| c.ok())
            .collect();
        if !cols.iter().any(|c| c == "home_root") {
            self.conn
                .execute("ALTER TABLE meta_path ADD COLUMN home_root TEXT", [])?;
        }
        self.conn
            .execute("UPDATE schema_version SET version = 8", [])?;
        Ok(())
    }
}
