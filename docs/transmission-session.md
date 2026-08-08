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

## Export

Write the catalog into a Transmission-style config root (`torrents/` + `resume/`, overwrites same infohash):

```bash
seedchamp export transmission /path/to/session --all
seedchamp export transmission /path/to/session --all --dry-run
```

Does not write `settings.json`. Incomplete torrents always export `progress.blocks`/`pieces` as `none` (block maps are not reconstructed from the catalog bitfield — recheck in Transmission). Complete torrents use `all`. Stop Transmission before writing into a live config dir.
