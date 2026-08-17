# WIP: dead and outdated paths

Temporary punch list for agents. Delete this file when every item is resolved (code gone, or marked keep with a reason in the row). Do not promote these rows into [roadmap.md](roadmap.md). Do not add this file to [README.md](README.md).

Survey: 2026-08-17. Architecture: [design.md](design.md). Modules: [domains.md](domains.md).

**Already done — do not re-open**

- Fake-async `disk::read_span` (async wrapper around blocking `pread`). Blocking `pread` is now `read_span`.
- Unused `upload::read_block` / `read_block_into` / `fill_spans_compio`. Seed fill is `begin_upload` only. Header helpers stay in `upload/read.rs`. Test `begin_upload_fills_payload` covers the live fill.

Those two are the template for everything below.

---

## Agent contract

1. **Confirm callers before deleting.** Grep the symbol across `crates/`, `src/`, `bench/`, `docs/`. In-tree “live” means TUI, CLI (`src/main.rs`), `serve` / `bench swarm`, import/export, or a non-test production call. Definition + `pub use` + unit test is not live.
2. **Do not implement the old design** to “fix” a docs-drift row. Update the doc or rustdoc to match the tree unless the user explicitly asks to build the missing behavior.
3. **Do not collapse live splits.** See [Do not treat as dead](#do-not-treat-as-dead). CLI vs TUI, Compio vs `pread`, rtorrent vs Transmission are product, not leftovers.
4. **One item or one tight cluster per change.** Prefer the next unchecked **Delete-ready** row. Do not drive-by delete unrelated leftover API in the same patch unless it is in the files you already have open and the user asked for a sweep.
5. **Comments document the function, not history.** Do not leave “formerly X” / “currently Y instead”.
6. **Ship only current code.** No compatibility shim, no `#[deprecated]`, no dual path “just in case”.
7. **Verify.** `cargo fmt --all`. Targeted `cargo test -p seedchamp-engine --lib <module>::` plus any crate that imported the symbol. Broader `cargo test` before a multi-item sweep. Smoke (`./bench/smoke.py` after `cargo build --release`) if you touched seed fill, disk, peer, or announce.
8. **Update this file in the same change.** Tick **Done**. If you keep something, replace the empty cell with `keep` and a one-line reason. When every Done/keep cell in every table is filled, delete this file. Do not leave an empty punch list in `docs/`.

Crate root re-exports live in `crates/engine/src/lib.rs`. Removing a `pub use` is a public-item change; nothing in this repo (TUI, CLI, import, bench) uses the leftovers listed here.

---

## Precedent (how the last two were done)

| Leftover | What we did |
|----------|-------------|
| `disk::read_span` async + `read_span_blocking` | Deleted the async wrapper. Renamed the blocking fn to `read_span`. Docs: this module is hash/recheck + upload `pread` backend. Compio fill stays in `begin_upload`. |
| `upload::read_block*` | Deleted the Compio-only reader. Kept `fill_piece_header` / `build_piece_header`. Pointed the test at `begin_upload` (`ResolvedUploadBackend::Compio`). Dropped `pub use` from `upload.rs`. |

If a leftover is “second implementation of a live job”, delete the unused one and move the test onto the live entry. Do not wire the leftover into production.

---

## Delete-ready (same class as `read_block`)

Live TUI / CLI / swarm does not call these. Callers are definition, re-export, or a unit test.

Pick the first unchecked row unless the user names one.

| Done | Item | Playbook |
|------|------|----------|
| x | Sync leech commit | [#1](#1-sync-leech-commit) |
| x | Headless serve wrapper | [#2](#2-headless-serve-wrapper) |
| x | Private Compio FD open | [#3](#3-private-compio-fd-open) |
| x | Cloned std read FD | [#4](#4-cloned-std-read-fd) |
| x | Layout wanted-piece | [#5](#5-layout-wanted-piece) |
| x | Metainfo file export | [#6](#6-metainfo-file-export) |
| x | Session byte counters | [#7](#7-session-byte-counters) |
| x | MSE leftover helpers | [#8](#8-mse-leftover-helpers) |
| x | Blocking control waits | [#9](#9-blocking-control-waits) |

### 1. Sync leech commit

**Leftover:** `commit_verified_piece`, and (optionally later) `PendingPiece`.

**Live path:** peer staging (`ActivePiece` + `StagingPool`) → `take_for_hash` → `HashPool` → SHA-1 on `seedchamp-hash-*` → `DiskWorker::submit_write`. Thread backend of DiskWorker still calls `write_piece`.

**Files**

- `crates/engine/src/staging/pool.rs` — `PendingPiece` (~174), `commit_verified_piece` (~639), test `assemble_verify_write` (~675)
- `crates/engine/src/staging.rs` — `pub use` of both
- `crates/engine/src/lib.rs` — re-exports `StagingPool` / `PieceBufferPool` only (not `commit_verified_piece`)

**Evidence:** `commit_verified_piece` is only called from `assemble_verify_write`. Production never hashes or writes in `staging/`.

**Do**

1. Delete `commit_verified_piece`.
2. Keep `write_piece` (`disk/write.rs`). DiskWorker thread path and benches need it.
3. Rewrite `assemble_verify_write`: assemble + SHA-1 on the buffer is enough, or write via `write_piece` after hash if you still want a round-trip. Do not call `HashPool` from this unit test unless you already have that harness.
4. Drop `commit_verified_piece` from `staging.rs`.

**PendingPiece:** test-only piece assembler (also `reject_clears_only_one_block` and one more test in the same module). Production uses `ActivePiece`. First change: delete the commit fn only. Migrating those tests onto `StagingPool` is a follow-up in [Smaller leftover API](#smaller-leftover-api) if you take it.

**Grep:** `commit_verified_piece`, `PendingPiece`.

**Verify:** `cargo test -p seedchamp-engine --lib staging::`

### 2. Headless serve wrapper

**Leftover:** `run_serve_loop`, `SeedHandle`.

**Live path:** `serve_main` in `library/run.rs`. CLI `serve` and `bench swarm` call it.

**Files**

- `crates/engine/src/library/seed.rs` — `SeedHandle`, `run_serve_loop` (~100). Wrapper: spawn thread `seedchamp-serve`, call `serve_main(..., Vec::new(), false, stop)`.
- `crates/engine/src/library.rs` — `pub use … run_serve_loop, SeedHandle`
- `crates/engine/src/lib.rs` — same names in the crate-root `pub use`

**Keep in `seed.rs`:** `PKG_VERSION`, `pkg_version_major`, `DEFAULT_PEER_ID_PREFIX`, `generate_peer_id_with_prefix`, `resolve_peer_id_prefix`, and the `peer_id_tests` module. `generate_peer_id` is a leftover of its own (session uses `generate_peer_id_with_prefix`); do not delete it in this item unless you also tick that leftover-API row.

**Do:** delete `SeedHandle` and `run_serve_loop`. Drop both from `library.rs` and `lib.rs`. Fix the module rustdoc (it still says “background seed-loop handle”).

**Grep:** `run_serve_loop`, `SeedHandle`.

**Verify:** `cargo test -p seedchamp-engine --lib library::` and `cargo test -p seedchamp --bin seedchamp` if CLI tests exist. `cargo run -- doctor`.

### 3. Private Compio FD open

**Leftover:** `FdCache::open_read_compio`.

**Live path:** `open_read_compio_peer` + `with_peer_fd_cache` from `upload/inflight.rs` `fill_payload_compio`. Compio files are `!Send`; peer workers are pinned; TLS is the share point.

**Files**

- `crates/engine/src/disk/fd_cache.rs` — `open_read_compio` (~139), test `open_read_compio_reuses` (~343). Twin test `open_read_compio_peer_tls_reuses` already covers TLS reuse.
- `compio_get` / `compio_insert` stay. They are used by `open_read_compio_peer`.

**Do:** delete `open_read_compio` and `open_read_compio_reuses`. Keep `open_read_compio_peer`. Do not export a “generic” Compio open.

**Grep:** `open_read_compio` (exclude `open_read_compio_peer`).

**Verify:** `cargo test -p seedchamp-engine --lib disk::fd_cache::`

### 4. Cloned std read FD

**Leftover:** `FdCache::open_read_cloned`.

**Live path:** blocking reads use `open_read` (`read_span`). Writes that need an owned FD (uring / aio) use `open_write_cloned`.

**Files:** `crates/engine/src/disk/fd_cache.rs` (~101) and test `open_read_cloned_keeps_cache_entry` (~316).

**Do:** delete the method and its test. **Do not** delete `open_write_cloned`.

**Grep:** `open_read_cloned`.

**Verify:** `cargo test -p seedchamp-engine --lib disk::fd_cache::`

### 5. Layout wanted-piece

**Leftover:** `StorageLayout::piece_wanted` in `crates/engine/src/disk/spans.rs` (~106).

**Live path:** `HotTorrent::build_wanted_bitfield` / `wants_piece` in `hot/pieces.rs`.

**Keep:** `FileLayout::wanted`, `FileLayout::end` (used by `spans_for_range` and priority skips).

**Do:** delete `piece_wanted` only.

**Grep:** `piece_wanted`.

**Verify:** `cargo test -p seedchamp-engine --lib disk::spans::` (and any `disk::` layout tests).

### 6. Metainfo file export

**Leftover:** `Catalog::export_torrent_file` in `crates/engine/src/catalog/torrent.rs` (~183). Writes `torrent_metainfo.blob` to a path.

**Live path:** `crates/import/src/export.rs` uses `get_metainfo_blob`. CLI is `seedchamp export rtorrent|transmission`.

**Do:** delete the method. There is no `torrent export` command to update.

**Grep:** `export_torrent_file`.

**Verify:** `cargo test -p seedchamp-engine --lib catalog::` and `cargo test -p seedchamp-import`.

### 7. Session byte counters

**Leftover:** `SessionRuntime::record_download`, `record_upload` in `session/catalog_io.rs` (~152).

**Live path**

- Upload: `PeerConfig::on_upload` → `torrent_bytes.up`. Announce reads `raw_uploaded`.
- Download session totals: `completed_bytes − baseline`, not `torrent_bytes.down`.
- `LivePeer.uploaded` / `downloaded` are constructed at 0 and never `fetch_add`. `LivePeer::up`/`down` use `wire_up`/`wire_down`. That pair is [Smaller leftover API](#smaller-leftover-api).

**Keep:** `torrent_bytes.up`, `raw_uploaded`, `on_upload`, `ensure_byte_counters`.

**Do:** delete `record_download` and `record_upload`. After that, if `torrent_bytes.down` is only written by the deleted methods, remove that field and the snapshot `tb_down` fallback in the same change if the compiler points at it — stay in this module.

**Grep:** `record_download`, `record_upload`.

**Verify:** `cargo test -p seedchamp-engine --lib session::`

### 8. MSE leftover helpers

**Live:** `initiator_scan_response`, `initiator_build_sync`, `receiver_parse_initiator`, `receiver_build_response` from `pe_initiate` / inbound PE.

**Leftover** (engine-internal; TUI/CLI/import never call them)

| Symbol | File | Callers |
|--------|------|---------|
| `finish_session` | `crypto/mse.rs` | none |
| `receiver_response_payload` | `crypto/mse.rs` | none |
| `validate_select` | `crypto/mse.rs` | none |
| `initiator_sync_payload` | alias of `initiator_build_sync` (`pub use … as`) | none |

`initiator_parse_response` and `handshake_loopback` are **tests only**. Leave them unless you rewrite `handshake_loopback` onto `initiator_scan_response` in the same change.

**Do:** delete the four unused symbols. Drop them from `crypto.rs` `pub use`. Keep `MseSession`, `Rc4`, `CRYPTO_*` crate-root exports.

**Grep:** each symbol name.

**Verify:** `cargo test -p seedchamp-engine --lib crypto::`

### 9. Blocking control waits

**Leftover:** `ControlHandle::start`, `stop`, `recheck` in `control/handle.rs` (~90–135). Each `request_*` then `wait_for` a matching event. Comments say “CLI / tests”.

**Live path**

- TUI: `request_start` / `request_stop` / `request_recheck` / … and polls events. Never blocking `start`.
- CLI: does **not** use `ControlHandle`. `torrent start|stop` is `Catalog::set_want_start`. `torrent recheck` is `recheck_torrent` (no session).
- Control tests: `request_*` + event wait, not these helpers.

**Do:** delete the three methods. Keep `request_*`, `try_recv_event`, and `wait_for` if tests still use `wait_for`; if `wait_for` becomes unused, delete it too. Keep `send` if `request_*` still use it.

**Do not** “fix” CLI by routing it through `ControlHandle`. Catalog-only CLI is [domains.md](domains.md).

**Grep:** `handle.start(`, `handle.stop(`, `handle.recheck(`, `ControlHandle::start` (watch false positives on `SessionRuntime::start` / `WatchHandle::stop`).

**Verify:** `cargo test -p seedchamp-engine --lib control::` and TUI if it compiled against those names (it should not).

---

## Smaller leftover API

Shrink visibility (`pub(crate)` / `#[cfg(test)]`) or delete. Not a second implementation. Batch a module at a time.

| Done | Symbol | Where | Notes |
|------|--------|-------|-------|
| x | `Catalog::torrent_count` | `catalog/open.rs` | `SELECT COUNT(*) … deleted=0`. No callers. |
| x | `Catalog::piece_count` | `catalog/pieces.rs` | No `.piece_count(` callers. |
| x | `Catalog::mark_piece_have` | `catalog/pieces.rs` | Wrapper. Session uses `mark_pieces_have_batch`. |
| x | `Catalog::get_bitfield_have_count` | `catalog/pieces.rs` | Tests in `catalog/queries.rs` and `hash/recheck.rs` only. |
| x | `Catalog::prune_peer_cache` | `catalog/peers.rs` | Tests only. Production is `prune_peer_cache_on` from `persist_after_announce`. |
| x | `TorrentStats` | `catalog/types.rs` | Re-exported from `catalog.rs`. Never constructed. |
| x | `InsertOutcome::is_new` | `catalog/queries.rs` | `id()` is used in tests. |
| x | `date_stamp` | `library/watch.rs` | Watch templates expand `{date}` inside `expand_dl_path_template`. Test `date_stamp_format` only. |
| x | `wanted_bytes_from_layout` | `library/leech_cache.rs` | Add uses `wanted_bytes_from_metainfo`. One unit test. Crate-root export. |
| x | `generate_peer_id` | `library/seed.rs` | Session uses `generate_peer_id_with_prefix`. Keep the prefix helper. |
| x | `PendingPiece` (after #1) | `staging/pool.rs` | Test assembler. Migrate remaining tests to `StagingPool` / `ActivePiece` or mark `#[cfg(test)]` and stop exporting. |
| x | `derive_peer_rc4` | `crypto/keys.rs` | Tests only. Live MSE: `rc4_key_a` / `rc4_key_b` + `Rc4::new_mse`. |
| x | `Rc4::crypt` | `crypto/rc4.rs` | Copy-then-inplace. Zero callers. Keep `crypt_inplace`. |
| keep — docs/wip-bep6-suggest.md | `encode_suggest_messages` | `wire/fast.rs` | Never send Suggest. Keep HAVE_ALL/NONE/Reject/Allowed Fast. |
| keep — docs/wip-bep6-suggest.md | `FastSession::{peer_allows_while_choked,we_allow_while_choking}` | `wire/fast.rs` | Unused predicates. `on_suggest` / `suggested` are written and never read — drop with Suggest-recv docs row. |
| x | `full_bitfield_bytes` | `wire/messages.rs` | Catalog uses `all_set_bitfield`. |
| x | `SessionRuntime::wire_limiter` | `session/limits.rs` | Zero callers. Peers get `inner.wire_limiter` via `PeerConfig`. |
| x | `LivePeer.uploaded` / `downloaded` | `session.rs` | Always 0. Accessors use `wire_up` / `wire_down`. |
| x | `allow_upload` / `commit_upload` (+ download) | `rate_limit.rs` | Tests only. Live: `try_consume_*`, `refund_*`, `*_delay_for`. |
| x | `HashPool::spawn` | `runtime/hash_worker.rs` | Session/tests use `spawn_n`. |
| x | `PeerWorkerPool::with_default_workers` | `runtime/pool.rs` | Session uses `PeerWorkerPool::new(workers)`. |
| x | `PeerWorkerPool::peer_counts` | `runtime/pool.rs` | Tests in `pool.rs` only. |
| x | `DiskWorker::spawn` | `runtime/disk_worker.rs` | Tests/peer harness. Session uses `spawn_with_options`. Keep `spawn_with_options`. |
| x | `FdCache::is_empty` | `disk/fd_cache.rs` | Defined, never called. |
| x | `HASH_READ_WINDOW` crate-root / `disk.rs` re-export | `lib.rs`, `disk.rs` | Only `disk/read.rs` uses the const. Can stay `pub` inside `read.rs`. |
| x | `recheck_torrent_with_progress` | `hash/recheck.rs` | Only `recheck_torrent` calls it with `\|_\| {}`. CLI does not stream progress. Keep `recheck_torrent`. |
| | `TransmissionResume.want_start_hint` | `crates/import/src/transmission/resume.rs` | From `paused`. `import_one` ignores it. Start is `--start-after`. |
| | `RtorrentSide.tied_to_file` | `crates/import/src/rtorrent_side.rs` | Parsed, never read. Design §8 still lists “tied file” — tick the docs row if you drop the field. |

**Verify per batch:** `cargo test -p seedchamp-engine --lib <module>::` and `cargo test -p seedchamp-import` if you touched import.

---

## Config that parses and does not drive runtime

`RuntimeConfig::from_config` (`session/config.rs`) copies tracker **caps** only: `max_concurrent_per_host`, `startup_stagger_ms`, `max_inflight_announces`.

For each row: **wire the field into the live path**, or **delete it from `Config`, the template, env overrides, and tests**. Do not leave a TOML key that looks live. Prefer delete unless the user asks to wire it.

| Done | Field / env | Declared | What runs |
|------|-------------|----------|-----------|
| x | `tracker.http_timeout_secs` | `config.rs` `TrackerConfig`, default 30 | Deleted. HTTP still uses 12s. |
| x | `tracker.numwant` | default 50 | Wired into `RuntimeConfig` / announce. |
| x | `tracker.min_interval_secs` | default 60 | Deleted. Announce constants stay. |
| x | `tracker.max_interval_secs` | default 3600 | Deleted. Announce constants stay. |
| x | `logging.level`, env `SEEDCHAMP_LOG` | `apply_env_overrides` writes the field | Deleted. TUI still hardcodes info. |
| x | Template `SEEDCHAMP_SEND_BUFFER=4M` | comment in `config.rs` template (~942) | Comment now `SEEDCHAMP_SEND_BUFFER_BYTES`. |

**Watch default (docs/template only):** `WatchConfig::interval_secs` default is **5**. Template comment still says `interval_secs=1`. Fix with the watch docs-drift row.

**Verify:** `cargo test -p seedchamp-engine --lib config::` and `cargo run -- doctor`.

---

## Docs drift

Update [design.md](design.md) / [domains.md](domains.md) / rustdoc to match the tree. **Do not implement** the old sentence unless the user asks.

| Done | Claim | Where | Code now |
|------|-------|-------|----------|
| | TUI list is paged SQL / “5k rows, no long hitch” | design §7, §10 | `Catalog::list_torrents_filtered` loads every non-deleted row. `control/reader.rs` sends a full `CatalogList`. PgUp/PgDn scroll memory. |
| | Refresh 2–5 Hz; 10–20 Hz focused torrent | design §7 | `SNAPSHOT_INTERVAL` 1s, `SQL_INTERVAL` 5s (`crates/tui/src/app/events.rs`). No focused high-Hz path. |
| | Hot activate on inbound / recheck; idle evict | design §4 | `start_torrent` / `sync_want_start` load hot. Only `stop_torrent` removes. Recheck updates catalog only. Inbound looks up existing hot; cold infohash stays cold. |
| | Piece hashes mmap/blob | design §4 | `Catalog::load_piece_hashes` → `Vec<u8>`; hot holds `Arc<Vec<u8>>`. |
| | `--upload-backend` general CLI | design §5, domains §3 | Clap flag only on `seedchamp bench swarm`. `serve` / TUI: `[upload].backend` + `SEEDCHAMP_UPLOAD_BACKEND`. |
| | Import scan is “48-char hex + `.torrent`” | design §8 | `is_infohash_torrent_name`: **40 hex** + `.torrent` = 48 total (`crates/import/src/common.rs`). Matches `rtorrent-session.md`. |
| | Import optional payload existence/size check | design §8 step 7 | Import inserts sidecars. No payload `stat`. |
| | SQLite file modes 0600 | design §11 | `Connection::open` only. No `chmod`. |
| | Config section list | design §9 | Omits `[catalog]` (`soft_delete_purge_days`). |
| | Palette relocate / import / export | design §7 | Relocate is **Ctrl-O** + `Mode::Relocate` only. No `:relocate`. Palette has no import/export. |
| | BEP 6 Suggest recv | design §5 | `on_suggest` stores; nothing reads `suggested`. We never send Suggest. |
| | `hash/` does leech verify | `hash.rs` rustdoc | Leech SHA-1 is `runtime/hash_worker.rs`. `hash/` is serial recheck. |
| | Staging assemble → SHA-1 → write | `staging.rs` / `staging/pool.rs` rustdoc | Staging assembles only. |
| | Hash read is `read_at` | design §6 table | Recheck/hash is blocking `pread` via `hash_piece_windowed`. Compio `read_at` is **seed fill**. |
| | Recheck is sequential windowed reads | design §6 | TUI/control: `recheck_torrent_with_pool`. CLI: sequential `recheck_torrent`. |
| | Watch template `interval_secs=1` | `config.rs` template | Default is 5. |

When you edit design/domains, follow AGENTS.md (those files change when the described architecture changes). This punch list is not architecture.

---

## Do not treat as dead

Do not delete one side. Do not “unify” without an explicit product request.

- CLI `torrent start|stop|recheck|del` is catalog-only. TUI uses the control plane. [domains.md](domains.md) §1.
- CLI serial `recheck_torrent` vs TUI `recheck_torrent_with_pool`.
- Seed fill: Compio `read_at` vs TLS `pread` inside `begin_upload` (`prefer_compio_fill`). Darwin / ZFS `auto` → `pread` is intentional.
- rtorrent and Transmission import/export.
- `StagingPool::empty` (seed, no RAM pool) vs `from_pool` (leech).
- Disk backends `thread` / `uring` / `aio`.
- Soft-delete (Ctrl-D / `torrent del`) vs hard-remove (`:remove`). Both require stopped (`want_start = 0`). Payload stays on disk.
- `write_piece` — live on DiskWorker **thread** backend.
- `read_span` — live blocking `pread` (hash/recheck + upload `pread` backend).
- `open_read_compio_peer` / `with_peer_fd_cache` — live Compio seed fill.
- `fill_piece_header` — live PIECE header.
- Encryption modes `off` / `prefer-plain` / `prefer-rc4` / `require-rc4` — all selected from config.

**Not unused, not in this list to implement:** `MAX_REQUEST_LENGTH` (128 KiB) vs `UPLOAD_BLOCK_LEN` (16 KiB). `classify_upload_request` can accept a block that `begin_upload` then rejects. Separate product decision.

---

## Suggested order

1. Delete-ready #1–#9 (smallest first if you want a warm-up: #5, #4, #6, then #3, #7, #2, #8, #9, #1).
2. Leftover API by module (`catalog/`, `crypto/`, `library/`, `wire/`, `session/`).
3. Config: delete unused keys unless asked to wire them.
4. Docs drift last (or with the code change that made the sentence false).

## Close-out

When every **Done** cell in every table is `x` or `keep …`:

1. Grep this file for leftover unchecked `| |` rows.
2. Delete `docs/wip-dead-paths.md`.
3. Do not add a “we cleaned dead paths” row to [roadmap.md](roadmap.md).
