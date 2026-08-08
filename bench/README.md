# seedchamp bench — smoke + throughput

In-tree localhost harness for seedchamp (sc↔sc, optional libtorrent-rasterbar).
Pure Python (plus one Rust example for DiskWorker).

| Entry | Role |
|-------|------|
| `smoke.py` | Correctness gate: crypto, multipeer, disk + upload backends, optional rasterbar PE/RC4 |
| `throughput.py` | Timed sc→sc MiB/s (durable / discard × backends) |
| `diskworker.py` | **DiskWorker writes only** — no peers/hash/network; wraps `disk_write_bench` |
| `common.py` | Shared process/port helpers |
| `lt_peer.py` | Python libtorrent-rasterbar seeder/leecher (trackerless) |
| `gen_seed.py` | Deterministic payload + `.torrent` |

## Quick start

```bash
cd seedchamp
cargo build --release

# default smoke (~2 MiB): crypto ×3, multipeer, disk matrix, upload auto/pread/compio, rasterbar if present
./bench/smoke.py

# dual binary (branch seeder vs master leecher)
./bench/smoke.py --seed-bin ./target/release/seedchamp \
                 --leech-bin /path/to/master/seedchamp

# throughput
./bench/throughput.py
./bench/throughput.py --size 1G --iters 5 --backends thread,uring --paths durable,discard
```

Minimal single-cell run:

```bash
./bench/smoke.py \
  --size 64K --piece-length 32K \
  --modes plain \
  --seeders 0 --leechers 0 \
  --backends auto \
  --upload-backends auto \
  --no-rasterbar \
  --timeout 45 \
  --keep-work
```

## Smoke matrix

1. **Crypto sc→sc** — modes `plain` / `handshake` / `rc4` (1 seeder → 1 leecher), SHA-256
2. **leech_cache** — leecher stages under `/tmp/{infohash}/`, downloads, handoff-copies to permanent `data_root`, deletes stage
3. **Multipeer N→1** — default 3 seeders → 1 leecher (plain)
4. **Multipeer 1→N** — default 1 seeder → 3 leechers (plain)
5. **Disk backend** — default **`matrix`** (OS-available: Linux `thread,uring`; FreeBSD/Darwin `thread,aio`); or `auto` / explicit list
6. **Upload backend** — default **`auto,pread,compio`** on the seeder (`--upload-backend`); cells `upload-auto` / `upload-pread` / `upload-compio`. Skip with `--upload-backends none`
7. **Rate limits** (default **2M only**) — seeder upload cap and leecher download cap via `SEEDCHAMP_MAX_*_BPS`; expect ~10s each (fail if too fast)
8. **Rasterbar** — if `import libtorrent` works: roles `lt-sc` × `sc-lt` × modes plain/handshake/rc4  
   Cell ids: `{role}-{mode}` e.g. `lt-sc-plain` (rasterbar seeds → sc leeches), `sc-lt-rc4` (sc seeds → rasterbar leeches).

Missing python bindings → rasterbar cells **skipped** (unless `--with-rasterbar`). Rate-limit cells **skipped** when `--size` / `--big` is not 2M.

### Useful flags

| Flag | Default |
|------|---------|
| `--size` / `--big [SIZE]` | 2M / 50M |
| `--modes` | plain,handshake,rc4 |
| `--seeders` / `--leechers` | 3 / 3 (0 skips that topology) |
| `--backends` | matrix (disk) |
| `--upload-backends` | auto,pread,compio |
| `--bin` / `--seed-bin` / `--leech-bin` | tree release/debug binary |
| `--with-rasterbar` / `--no-rasterbar` | auto |
| `--lt-modes` / `--lt-roles` | all / lt-sc,sc-lt |
| `--port-base` | 53810 (smoke) / 53910 (throughput) |
| `--keep-work` | off (work under `bench/work/`) |

Env: `SEEDCHAMP_BIN`, `SEEDCHAMP_SEED_BIN`, `SEEDCHAMP_LEECH_BIN`,
`SEEDCHAMP_DISK_BACKEND`, `SEEDCHAMP_DISK_DEPTH`, `WORK` (via `--work`), `PORT_BASE` (via `--port-base`).

## Throughput

Default **100M**, 1 warmup + 3 iters, durable path, backend `auto`.

```text
backend=uring path=durable label=run-1 elapsed_s=… rate_MBps=… status=ok …
median backend=uring path=durable rate_MBps=…
```

## DiskWorker write bench

Isolates leech durable writes: `submit_write` → thread / io_uring / aio. No
BitTorrent stack. Piece bytes are per-index xorshift (incompressible). Rust
example: `crates/engine/examples/disk_write_bench.rs`.

```bash
# default: OS matrix × durable,discard × depth 32, 256M @ 1M pieces
./bench/diskworker.py --build

./bench/diskworker.py --backends thread,uring --paths durable --depths 1,32,128 --size 1G
./bench/diskworker.py --layout multi --paths durable --size 256M

# one cell directly
cargo run -p seedchamp-engine --example disk_write_bench --release -- \
  --backend uring --path durable --size 512M --depth 32
```

Result line (one per cell):

```text
backend=io_uring want=uring path=durable depth=32 piece=1048576 layout=single \
  pieces=… written=… elapsed_s=… rate_MBps=… p50_us=… p99_us=… status=ok …
```

| Flag | Default |
|------|---------|
| `--backends` | matrix (OS-available) |
| `--paths` | durable,discard |
| `--depths` | 32 |
| `--size` / `--piece-length` | 256M / 1M |
| `--layout` | single (or `multi` = dual-span every piece) |

## Rasterbar install

| OS | Package (examples) |
|----|--------------------|
| FreeBSD | `pkg install py312-libtorrent-rasterbar` |
| Debian/Ubuntu | `python3-libtorrent` |
| From source | build bindings for system libtorrent-rasterbar |

```bash
python3 -c 'import libtorrent as lt; print(lt.__version__ if hasattr(lt,"__version__") else "ok")'
```

Workdir is `bench/work/` (gitignored). Python 3 required.
