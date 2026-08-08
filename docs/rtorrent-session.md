# rtorrent session layout (import)

rtorrent / libtorrent session layout (`DownloadStorer`):

```text
$session/
  <40-hex-infohash>.torrent
  <40-hex-infohash>.torrent.rtorrent
  <40-hex-infohash>.torrent.libtorrent_resume
```

Optional: `.meta` for magnet metadata downloads.

## Mapping (design §8)

| File / key | seedchamp catalog |
|------------|-------------------|
| `.torrent` metainfo | `torrent`, `torrent_file`, `piece_hashes`, trackers, **`torrent_metainfo.blob`** (exact file) |
| `.libtorrent_resume` | `bitfield`, priorities, uploaded/downloaded |
| `.rtorrent` | `directory` → data_root; `timestamp.started` / `state_changed` → `created_at`; `timestamp.finished` → `finished_at`; `total_uploaded` / `total_downloaded` (max with resume) |

**Multi-file layout:** seedchamp stores paths as `TorrentName/…` under `data_root` (BEP 3). rtorrent’s session `directory` already includes that name — import strips it so paths do not double-nest.

Re-running `import rtorrent` on an existing catalog **updates** timestamps/stats for already-imported torrents (does not duplicate).

```bash
seedchamp import rtorrent /path/to/session --db data/catalog.sqlite
seedchamp import rtorrent /path/to/session --dry-run
```
