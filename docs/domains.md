# seedchamp domains

Domain boundaries, dependency rules, and naming. Architecture: [design.md](design.md). Open work: [roadmap.md](roadmap.md).

| | |
|--|--|
| **Interfaces** | TUI and CLI drive one `SessionRuntime` |

---

## 1. Interfaces

```text
  seedchamp (TUI default)  ──┐
  seedchamp serve            ├──► SessionRuntime + control ──► hot / peer / disk / tracker …
  seedchamp torrent|import|… ──┘
```

| Interface | Entry | Role |
|-----------|--------|------|
| **TUI** | `seedchamp` / `seedchamp tui` | Interactive control plane (mutations via command bus) + snapshots |
| **CLI** | `serve`, `torrent …`, `import\|export rtorrent\|transmission`, `watch`, `doctor`, `config`, `bench`, `version` | Non-interactive; `torrent start|stop|recheck|del` are catalog-only; swarm via `serve` / TUI |

No separate seed engine vs leech engine. Activation is **`want_start`**. Incomplete torrents seed what they have; complete torrents seed only.

**Seed-while-leech:** always on for active incomplete torrents. Exception: `discard_writes` (bench) must not serve discarded payload.

---

## 2. Domain map

| Domain | Responsibility | Module |
|--------|----------------|--------|
| **metainfo** | bencode + `.torrent` parse | `metainfo` (+ `bencode`) |
| **catalog** | SQLite session authority | `catalog/` |
| **disk** | spans, fd cache (disk thread + per peer-worker TLS for seed fill), pwrite/pread | `disk/` |
| **hash** | recheck algorithm | `hash/` |
| **wire** | BT messages, peer id, Fast | `wire/` |
| **crypto** | MSE/PE, RC4 | `crypto/` |
| **upload** | piece serve I/O | `upload/` |
| **staging** | leech piece assembly | `staging/` |
| **tracker** | HTTP/UDP announce | `tracker/` |
| **hot** | in-memory active torrent | `hot/` |
| **peer** | one TCP connection | `peer/` |
| **session** | accept, dial, announce, snapshots | `session/` |
| **control** | TUI command bus (mutate + catalog reader) | `control/` |
| **rate_limit** | Global up/down token buckets | `rate_limit` |
| **process_metrics** | Status-screen process / thread samples | `process_metrics` |
| **runtime** | peer I/O workers, disk thread, hash pool | `runtime/` |
| **library** | add, watch, headless `serve_main` | `library/` |
| **config** | XDG merge → pure types | `config` (no session impl dep) |

TOML `[swarm]` / `SwarmConfig` is process knobs (workers, pipeline), not a code module.

### Dependency direction

```text
cli / tui
  → control, library, config, catalog (read), session (API / snapshots)
session / control
  → hot, peer, tracker, runtime, catalog, crypto, upload, staging, disk
peer
  → wire, crypto, upload, staging, hot, disk
hot
  → catalog, disk (layout), bitfield helpers
catalog
  → metainfo types, disk layout types
config
  → pure types only; RuntimeConfig at session boundary
disk / crypto / wire / bencode
  → error (leaves)
```

**Forbidden:** `config` → session implementation; `catalog` → peer/session; `disk` → session; TUI → peer sockets or SQLite across network waits; interactive TUI mutations that bypass control when a control plane is required.

---

## 3. Upload and disk I/O

| Platform | Seed fill (`[upload].backend`) | Leech write |
|----------|--------------------------------|-------------|
| **Linux** | Peer seed fill: Compio **`read_at`** on ext4/xfs/btrfs (`auto`); **pread** on ZFS/etc.; `compio` force Compio | **DiskWorker** (`io_uring` or thread `pwrite`) |
| **FreeBSD** | Peer seed fill: Compio **`read_at`** (`auto`); `compio` / `pread` overrides | **DiskWorker** (POSIX **AIO** or thread `pwrite`) |
| **Darwin (macOS)** | Peer seed fill: **pread** (`auto`); `compio` force Compio | **DiskWorker** (POSIX **AIO** or thread `pwrite`) |

**Runtime (all platforms):** Compio for accept / peer / tracker networking. Topology: accept (`seedchamp-acc`) → least-peers → N peer workers (`seedchamp-io`); tracker (`seedchamp-trk`, cyper HTTP + Compio UDP). Catalog offloads via Compio `spawn_blocking` or dedicated threads. Details: [design.md](design.md) §3 and `crates/engine/src/{session,peer,runtime}/`.

- **Config:** `[upload].backend` / `SEEDCHAMP_UPLOAD_BACKEND` / `--upload-backend`: `auto` \| `pread` \| `compio`.
- **RC4:** fill → encrypt → write.
- **Leech writes:** `[disk] backend` / `depth` (default **32**); env `SEEDCHAMP_DISK_BACKEND` / `SEEDCHAMP_DISK_DEPTH`. Worst-case piece buffers ≈ **2×depth**.
- **`paths.leech_cache`:** optional stage for wanted downloads that fit free space and optional **`paths.leech_cache_size`**; handoff to permanent `data_root` when wanted complete (same path as Ctrl-O relocate).
- **Default encryption:** `prefer-plain`.

---

## 4. Naming

| Name | Meaning |
|------|---------|
| `serve_main` | Headless `serve` / `bench swarm` on `SessionRuntime` |
| `DiskWorker` | Verified piece writes (io_uring / aio / thread) |
| `peer/` | Per-connection protocol |
| `wire/` | BT messages / peer id / Fast |
| `HotTorrent` / `HotRegistry` | Hot working set |
| `want_start` (DB) | User wants torrent active |
| `SessionRuntime` | Process swarm |

---

## 5. Engine tree

```text
crates/engine/src/
  lib.rs
  error.rs
  catalog/
  control/               # plane, mutate, reader (seedchamp-cread), handle
  disk/
  hash/
  wire/
  crypto/
  upload/
  staging/
  tracker/
  hot/
  peer/
  session/
  runtime/               # PeerWorkerPool, DiskWorker, HashPool
  library/               # add, watch, serve_main
  config.rs
  net.rs
  rate_limit.rs
  process_metrics.rs
  activity_log.rs, bench.rs, metainfo.rs, bencode.rs
```

---

## 6. Catalog

**Schema:** [`crates/engine/src/catalog/schema.sql`](../crates/engine/src/catalog/schema.sql). Soft-delete / hot-vs-cold: [design.md](design.md) §4. Activation flag: `want_start`.

---

## 7. Verify

```bash
cargo fmt --all -- --check
cargo test
cargo build --release
cargo run -- doctor
./bench/smoke.py   # after release build
```
