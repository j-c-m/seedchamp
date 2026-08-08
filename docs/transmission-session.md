# Transmission session layout (import)

Standard Transmission config/session root (`torrents/` + `resume/`):

```text
$session/
  torrents/
    <40-hex-infohash>.torrent
  resume/
    <40-hex-infohash>.resume
  settings.json          # ignored by import
```

Typical roots: `~/.config/transmission-daemon/`, `~/.config/transmission/`.

## Mapping

| File / key | seedchamp catalog |
|------------|-------------------|
| `torrents/*.torrent` | `torrent`, files, piece hashes, trackers, **`torrent_metainfo.blob`** |
| `resume` `destination` | `meta_path.data_root` (multi-file name stripped if needed) |
| `resume` `progress.blocks` | complete when `all`; partial block maps → recheck (not piece bitfield) |
| `resume` `progress.pieces` | checked pieces only — **not** used as have-complete |
| `resume` `uploaded` / `downloaded` | `stats` |
| `resume` `priority` / `dnd` | `torrent_file.priority` |
| `resume` `added_date` / `done_date` | `created_at` / `finished_at` |

Re-running `import transmission` on an existing catalog **updates** timestamps/stats for already-imported torrents (does not duplicate).

```bash
seedchamp import transmission ~/.config/transmission-daemon
seedchamp import transmission /path/to/session --dry-run
seedchamp import transmission /path/to/session --start-after --data-root ~/downloads
```
