# seedchamp

**The Seed Champion** — a high-performance BitTorrent client built for seedboxes that refuse to compromise. Seed massive libraries without drowning in RAM: the catalog lives in SQLite, and only active torrents hold wire state. When you need to rip data in, pile on an SSD leech cache and huge memory buffers for ultra-high-speed leech.

Use the terminal UI day to day, or run headless with `serve`. Same engine either way. Linux, FreeBSD, and macOS.

## Why seedchamp

- **Large libraries, efficient resources** — thousands of torrents in the catalog without densifying idle ones
- **Seedbox first** — headless serve, rate limits, watch dirs, rtorrent / Transmission session import
- **Fast I/O path** — Compio-based networking; platform-aware upload and disk backends
- **Terminal native** — ratatui list, detail, peers, files, and activity log

It talks to trackers only (no DHT or PEX). Magnets, WebUI, and Windows are out of scope.

## Install

Rust toolchain required. Build from source:

```bash
cargo build --release
```

Binary: `./target/release/seedchamp` (or put it on your `PATH`).

## Quick start

```bash
seedchamp config init
seedchamp torrent add ./something.torrent --start
seedchamp
```

| | Default |
|--|---------|
| Config | `~/.config/seedchamp/config.toml` |
| Data | `~/.local/share/seedchamp/` |
| Listen | `0.0.0.0:6881` |

Open the firewall / forwarded port for your listen address. Run `seedchamp doctor` if something looks wrong.

## Everyday use

**TUI** — `seedchamp` (or `seedchamp tui`). Press `?` for the full key map.

| Key | Action |
|-----|--------|
| `j` / `k` | Move |
| `Enter` | Detail |
| `Ctrl+s` | Start / stop |
| `p` / `f` / `l` | Peers / files / log |
| `/` | Filter |
| `:` | Commands (`:add`, `:remove`, `:limits`, …) |
| `Ctrl+d` | Soft-delete (torrent must be stopped; files on disk stay) |
| `Ctrl+q` | Quit |

**Headless** — start the torrents you want, then:

```bash
seedchamp serve
```

**Watch folders** — configure `[watch]` in the config file, or run `seedchamp watch` without the UI.

**Moving from rtorrent**

```bash
seedchamp import rtorrent /path/to/rtorrent/session
seedchamp import rtorrent /path/to/rtorrent/session --dry-run
```

Details: [docs/rtorrent-session.md](docs/rtorrent-session.md).

**Moving from Transmission**

```bash
seedchamp import transmission ~/.config/transmission-daemon
seedchamp import transmission /path/to/session --dry-run
```

Details: [docs/transmission-session.md](docs/transmission-session.md).

## CLI cheatsheet

```bash
seedchamp torrent add ./film.torrent
seedchamp torrent add https://example.com/a.torrent --start
seedchamp torrent list
seedchamp torrent start <id-or-infohash-prefix>
seedchamp torrent stop  <id-or-infohash-prefix>
seedchamp torrent del   <id-or-infohash-prefix>
seedchamp torrent recheck <id-or-infohash-prefix>
seedchamp torrent --json list

seedchamp config show
seedchamp doctor
seedchamp version
```

Global flags: `--config PATH`, `--db PATH`.

## Configuration

```bash
seedchamp config init    # commented template
seedchamp config show    # effective config after CLI/env/file
```

Settings resolve in order: CLI flags, `SEEDCHAMP_*` environment variables, the config file, then built-ins.

Encryption mode (`network.encryption`): `prefer-plain` by default; also `off`, `prefer-rc4`, `require-rc4`.

The init template covers paths, peer limits, upload/disk backends, watch dirs, rate limits, and optional leech cache (stage downloads on a fast volume before the permanent data root).

## Docs

| | |
|--|--|
| [docs/design.md](docs/design.md) | Architecture |
| [docs/domains.md](docs/domains.md) | Modules and I/O |
| [docs/roadmap.md](docs/roadmap.md) | Open work |
| [docs/rtorrent-session.md](docs/rtorrent-session.md) | rtorrent import |
| [bench/README.md](bench/README.md) | Smoke and throughput harness |

## Development

```bash
cargo fmt --all
cargo build --release
cargo test
cargo run -- doctor
./bench/smoke.py
```

## License

[MIT](LICENSE.md).
