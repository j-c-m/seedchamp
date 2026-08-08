-- seedchamp catalog schema v1
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS schema_version (
  version INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS torrent (
  id            INTEGER PRIMARY KEY,
  infohash      BLOB NOT NULL UNIQUE,
  name          TEXT NOT NULL,
  piece_length  INTEGER NOT NULL,
  piece_count   INTEGER NOT NULL,
  total_size    INTEGER NOT NULL,
  state         TEXT NOT NULL DEFAULT 'stopped',
  want_start    INTEGER NOT NULL DEFAULT 0,
  complete      INTEGER NOT NULL DEFAULT 0,
  private       INTEGER NOT NULL DEFAULT 0,
  -- Soft-delete: 1 = hidden from UI/list; payload kept on disk
  deleted       INTEGER NOT NULL DEFAULT 0,
  -- Unix seconds when soft-deleted (NULL if not deleted); used for catalog purge
  deleted_at    INTEGER,
  -- rtorrent-style announce key (uint32, never 0); HTTP &key= / UDP key field
  tracker_key   INTEGER NOT NULL DEFAULT 0,
  created_at    INTEGER NOT NULL,
  error_msg     TEXT
);

CREATE TABLE IF NOT EXISTS torrent_file (
  torrent_id    INTEGER NOT NULL REFERENCES torrent(id) ON DELETE CASCADE,
  idx           INTEGER NOT NULL,
  path          TEXT NOT NULL,
  size          INTEGER NOT NULL,
  offset        INTEGER NOT NULL,
  priority      INTEGER NOT NULL DEFAULT 1,
  PRIMARY KEY (torrent_id, idx)
);

CREATE TABLE IF NOT EXISTS piece_hashes (
  torrent_id    INTEGER PRIMARY KEY REFERENCES torrent(id) ON DELETE CASCADE,
  hashes        BLOB NOT NULL
);

CREATE TABLE IF NOT EXISTS bitfield (
  torrent_id    INTEGER PRIMARY KEY REFERENCES torrent(id) ON DELETE CASCADE,
  bits          BLOB,
  have_count    INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS stats (
  torrent_id    INTEGER PRIMARY KEY REFERENCES torrent(id) ON DELETE CASCADE,
  uploaded      INTEGER NOT NULL DEFAULT 0,
  downloaded    INTEGER NOT NULL DEFAULT 0,
  corrupted     INTEGER NOT NULL DEFAULT 0,
  active_time   INTEGER NOT NULL DEFAULT 0,
  finished_at   INTEGER
);

CREATE TABLE IF NOT EXISTS tracker (
  id            INTEGER PRIMARY KEY,
  torrent_id    INTEGER NOT NULL REFERENCES torrent(id) ON DELETE CASCADE,
  url           TEXT NOT NULL,
  tier          INTEGER NOT NULL DEFAULT 0,
  enabled       INTEGER NOT NULL DEFAULT 1,
  -- Last announce result (NULL until first contact)
  seeders            INTEGER,
  leechers           INTEGER,
  last_announce_at   INTEGER,
  last_interval      INTEGER,
  last_peers         INTEGER,
  last_status        TEXT
);

CREATE TABLE IF NOT EXISTS peer_cache (
  torrent_id    INTEGER NOT NULL REFERENCES torrent(id) ON DELETE CASCADE,
  addr          BLOB NOT NULL,
  last_seen     INTEGER NOT NULL,
  flags         INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (torrent_id, addr)
);

CREATE TABLE IF NOT EXISTS setting (
  key           TEXT PRIMARY KEY,
  value         TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS meta_path (
  torrent_id     INTEGER PRIMARY KEY REFERENCES torrent(id) ON DELETE CASCADE,
  data_root      TEXT NOT NULL,
  -- Permanent library root when data_root is under paths.leech_cache (NULL/empty = not staged).
  home_root      TEXT,
  source_torrent TEXT
);

-- Exact original .torrent bytes (infohash-preserving export).
CREATE TABLE IF NOT EXISTS torrent_metainfo (
  torrent_id INTEGER PRIMARY KEY REFERENCES torrent(id) ON DELETE CASCADE,
  blob       BLOB NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_torrent_state ON torrent(state);
CREATE INDEX IF NOT EXISTS idx_torrent_complete ON torrent(complete);
CREATE INDEX IF NOT EXISTS idx_torrent_name ON torrent(name);
CREATE INDEX IF NOT EXISTS idx_torrent_deleted ON torrent(deleted);
CREATE INDEX IF NOT EXISTS idx_torrent_deleted_at ON torrent(deleted_at);
-- Hot paths: want_start sync, peer_cache list/prune by last_seen, tracker announce UPDATE
CREATE INDEX IF NOT EXISTS idx_torrent_want_start_deleted ON torrent(want_start, deleted);
CREATE INDEX IF NOT EXISTS idx_peer_cache_torrent_seen ON peer_cache(torrent_id, last_seen DESC);
CREATE INDEX IF NOT EXISTS idx_tracker_torrent_url ON tracker(torrent_id, url);
