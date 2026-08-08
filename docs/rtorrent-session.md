# rtorrent session layout (import)

rtorrent / libtorrent session layout (`DownloadStorer`):

```text
$session/
  <40-HEX-INFOHASH>.torrent
  <40-HEX-INFOHASH>.torrent.rtorrent
  <40-HEX-INFOHASH>.torrent.libtorrent_resume
```

Filenames use **uppercase** hex (rtorrent/`hash_string_to_hex`). Import accepts either case.

Optional: `.meta` for magnet metadata downloads.

## Mapping (design §8)

| File / key | seedchamp catalog |
|------------|-------------------|
| `.torrent` metainfo | `torrent`, `torrent_file`, `piece_hashes`, trackers, **`torrent_metainfo.blob`** (exact file) |
| `.libtorrent_resume` | `bitfield`, priorities, uploaded/downloaded |
| `.rtorrent` | `directory` → data_root; `timestamp.started` / `state_changed` → `created_at`; `timestamp.finished` → `finished_at`; `total_uploaded` / `total_downloaded` (max with resume); **export:** `state` ← `want_start` (`1` started / `0` stopped) |

**Multi-file layout:** seedchamp stores paths as `TorrentName/…` under `data_root` (BEP 3). rtorrent’s session `directory` already includes that name — import strips it so paths do not double-nest.

Re-running `import rtorrent` on an existing catalog **updates** timestamps/stats for already-imported torrents (does not duplicate).

```bash
seedchamp import rtorrent /path/to/session --db data/catalog.sqlite
seedchamp import rtorrent /path/to/session --dry-run
```

## Export

Write the catalog back to a flat rtorrent session (overwrites same infohash files):

```bash
seedchamp export rtorrent /path/to/session --all
seedchamp export rtorrent /path/to/session --all --dry-run
```

Requires stored metainfo blobs. Incomplete torrents without a piece map export empty resume bitfields (recheck in rtorrent if needed). Stop rtorrent before writing into its live session dir.
