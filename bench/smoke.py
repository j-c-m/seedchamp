#!/usr/bin/env python3
"""seedchamp/bench smoke — sc↔sc crypto, leech_cache, multipeer, disk/upload backends, optional rasterbar."""

from __future__ import annotations

import argparse
import shutil
import sqlite3
import sys
import time
from pathlib import Path

# Allow `python3 bench/smoke.py` from seedchamp root.
_BENCH = Path(__file__).resolve().parent
if str(_BENCH) not in sys.path:
    sys.path.insert(0, str(_BENCH))

from common import (  # noqa: E402
    BENCH_DIR,
    LT_PEER,
    BenchError,
    PortAllocator,
    ProcessRegistry,
    bins_banner,
    default_piece_for_size,
    file_size,
    gen_seed_payload,
    hardlink_or_copy,
    have_libtorrent_py,
    install_cleanup_handlers,
    parse_list_arg,
    parse_size_bytes,
    port_listening,
    resolve_backend_list,
    resolve_bins,
    sc_add,
    sc_recheck,
    sha256_file,
    start_sc_swarm,
    tail_file,
    wait_complete_log,
    wait_listen,
    wait_log_contains,
)


def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(
        description="seedchamp localhost smoke (sc↔sc + optional python libtorrent-rasterbar)"
    )
    p.add_argument("--size", default="2M", help="Payload size (default: 2M)")
    p.add_argument(
        "--big",
        nargs="?",
        const="50M",
        metavar="SIZE",
        help="Larger smoke (default 50M if bare --big)",
    )
    p.add_argument("--piece-length", default=None, help="Piece length (default by size)")
    p.add_argument(
        "--modes",
        default="plain,handshake,rc4",
        help="sc crypto modes (default: plain,handshake,rc4)",
    )
    p.add_argument("--seeders", type=int, default=3, help="N seeders → 1 leecher (0=skip)")
    p.add_argument("--leechers", type=int, default=3, help="1 seeder → N leechers (0=skip)")
    p.add_argument(
        "--backends",
        default="matrix",
        help="disk matrix | auto | thread,uring,... (default matrix = OS-available)",
    )
    p.add_argument(
        "--upload-backends",
        default="auto,pread,compio",
        help="seeder --upload-backend list (default auto,pread,compio; empty/none = skip)",
    )
    p.add_argument("--timeout", type=float, default=None, help="Per-cell timeout seconds")
    p.add_argument("--port-base", type=int, default=53810)
    p.add_argument(
        "--work",
        type=Path,
        default=None,
        help="Work directory (default: bench/work/smoke)",
    )
    p.add_argument("--bin", default=None, help="Both roles")
    p.add_argument("--seed-bin", default=None)
    p.add_argument("--leech-bin", default=None)
    p.add_argument("--build", action="store_true", help="cargo build release for default bin")
    p.add_argument("--debug", action="store_true", help="cargo build debug")
    p.add_argument(
        "--with-rasterbar",
        dest="rasterbar",
        action="store_const",
        const="on",
        default="auto",
    )
    p.add_argument(
        "--no-rasterbar",
        dest="rasterbar",
        action="store_const",
        const="off",
    )
    p.add_argument("--lt-modes", default="plain,handshake,rc4")
    p.add_argument("--lt-roles", default="lt-sc,sc-lt")
    p.add_argument("--pipeline", type=int, default=32)
    p.add_argument("--keep-work", action="store_true")
    return p


class Smoke:
    def __init__(self, args: argparse.Namespace) -> None:
        self.args = args
        if args.big:
            args.size = args.big
        self.size = args.size
        self.piece = args.piece_length or default_piece_for_size(self.size)
        self.bytes = parse_size_bytes(self.size)
        self.timeout = args.timeout
        if self.timeout is None:
            self.timeout = max(60.0, 60.0 + self.bytes / (1024 * 1024))
        self.modes = parse_list_arg(args.modes)
        self.lt_modes = parse_list_arg(args.lt_modes)
        self.lt_roles = parse_list_arg(args.lt_roles)
        self.work: Path = args.work or (BENCH_DIR / "work" / "smoke")
        self.reg = ProcessRegistry()
        install_cleanup_handlers(self.reg)
        self.ports = PortAllocator(args.port_base)
        self.pass_n = 0
        self.fail_n = 0
        self.skip_n = 0
        self.name = "smoke-seed"
        self.seed_bin: Path
        self.leech_bin: Path
        self.torrent: Path
        self.payload: Path
        self.expect_sha: str

    def cell_pass(self, msg: str) -> None:
        print(f"  PASS {msg}")
        self.pass_n += 1

    def cell_fail(self, msg: str) -> None:
        print(f"  FAIL {msg}", file=sys.stderr)
        self.fail_n += 1

    def cell_skip(self, msg: str) -> None:
        print(f"  SKIP {msg}")
        self.skip_n += 1

    def alloc_port(self) -> int:
        p = self.ports.alloc()
        self.reg.register_port(p)
        return p

    def sc_sc_transfer(
        self,
        cell: str,
        enc: str,
        backend: str | None = None,
        upload_backend: str | None = None,
    ) -> None:
        cdir = self.work / "cells" / cell
        self.reg.cleanup()
        if cdir.exists():
            shutil.rmtree(cdir)
        (cdir / "seed" / "data").mkdir(parents=True)
        (cdir / "leech" / "data").mkdir(parents=True)
        (cdir / "log").mkdir(parents=True)

        sport = self.alloc_port()
        lport = self.alloc_port()
        hardlink_or_copy(self.payload, cdir / "seed" / "data" / f"{self.name}.bin")
        stid = sc_add(
            self.seed_bin,
            cdir / "seed" / "catalog.sqlite",
            self.torrent,
            cdir / "seed" / "data",
        )
        sc_recheck(self.seed_bin, cdir / "seed" / "catalog.sqlite", stid)
        ltid = sc_add(
            self.leech_bin,
            cdir / "leech" / "catalog.sqlite",
            self.torrent,
            cdir / "leech" / "data",
        )

        seed_extra: list[str] = []
        if upload_backend:
            seed_extra.extend(["--upload-backend", upload_backend])

        sp = start_sc_swarm(
            self.reg,
            bin_path=self.seed_bin,
            db=cdir / "seed" / "catalog.sqlite",
            enc=enc,
            listen=f"127.0.0.1:{sport}",
            tid=stid,
            log_path=cdir / "log" / "seeder.log",
            backend=backend,
            extra=seed_extra,
        )
        if not wait_listen("127.0.0.1", sport, sp, 20):
            self.cell_fail(f"{cell} (seeder listen)")
            tail_file(cdir / "log" / "seeder.log", 20)
            self.reg.cleanup()
            return

        start_sc_swarm(
            self.reg,
            bin_path=self.leech_bin,
            db=cdir / "leech" / "catalog.sqlite",
            enc=enc,
            listen=f"127.0.0.1:{lport}",
            tid=ltid,
            log_path=cdir / "log" / "leecher.log",
            backend=backend,
            extra=[
                "--peer",
                f"127.0.0.1:{sport}",
                "--pipeline",
                str(self.args.pipeline),
            ],
        )
        leech_payload = cdir / "leech" / "data" / f"{self.name}.bin"
        if wait_complete_log(
            cdir / "log" / "leecher.log",
            self.timeout,
            self.bytes,
            leech_payload,
        ):
            got = sha256_file(leech_payload)
            if got == self.expect_sha:
                notes = []
                if backend:
                    notes.append(f"disk={backend}")
                if upload_backend:
                    notes.append(f"upload={upload_backend}")
                note = f" {' '.join(notes)}" if notes else ""
                self.cell_pass(f"{cell}{note}")
            else:
                self.cell_fail(f"{cell} (sha mismatch)")
        else:
            self.cell_fail(f"{cell} (timeout/incomplete)")
            tail_file(cdir / "log" / "leecher.log", 30)
        self.reg.cleanup()

    def sc_sc_rate_limit(
        self,
        cell: str,
        *,
        upload_bps: int | None = None,
        download_bps: int | None = None,
        target_secs: float = 10.0,
    ) -> None:
        """sc→sc plain with a global wire cap; expect ~target_secs for default 2M.

        Caps only on the constrained role (seeder upload or leecher download).
        Completing too fast means the limit was not enforced.
        """
        cdir = self.work / "cells" / cell
        self.reg.cleanup()
        if cdir.exists():
            shutil.rmtree(cdir)
        (cdir / "seed" / "data").mkdir(parents=True)
        (cdir / "leech" / "data").mkdir(parents=True)
        (cdir / "log").mkdir(parents=True)

        sport = self.alloc_port()
        lport = self.alloc_port()
        hardlink_or_copy(self.payload, cdir / "seed" / "data" / f"{self.name}.bin")
        stid = sc_add(
            self.seed_bin,
            cdir / "seed" / "catalog.sqlite",
            self.torrent,
            cdir / "seed" / "data",
        )
        sc_recheck(self.seed_bin, cdir / "seed" / "catalog.sqlite", stid)
        ltid = sc_add(
            self.leech_bin,
            cdir / "leech" / "catalog.sqlite",
            self.torrent,
            cdir / "leech" / "data",
        )

        seed_env: dict[str, str] = {}
        leech_env: dict[str, str] = {}
        if upload_bps is not None:
            seed_env["SEEDCHAMP_MAX_UPLOAD_BPS"] = str(upload_bps)
        if download_bps is not None:
            leech_env["SEEDCHAMP_MAX_DOWNLOAD_BPS"] = str(download_bps)

        # Timeout: target + headroom (burst + setup), hard ceiling ~2.5× target.
        cell_timeout = max(self.timeout, target_secs * 2.5 + 5.0)
        # Unlimited localhost 2M is usually well under 2s; require a clear slowdown.
        min_secs = max(3.0, target_secs * 0.4)

        sp = start_sc_swarm(
            self.reg,
            bin_path=self.seed_bin,
            db=cdir / "seed" / "catalog.sqlite",
            enc="plain",
            listen=f"127.0.0.1:{sport}",
            tid=stid,
            log_path=cdir / "log" / "seeder.log",
            env_extra=seed_env or None,
        )
        if not wait_listen("127.0.0.1", sport, sp, 20):
            self.cell_fail(f"{cell} (seeder listen)")
            tail_file(cdir / "log" / "seeder.log", 20)
            self.reg.cleanup()
            return

        t0 = time.monotonic()
        start_sc_swarm(
            self.reg,
            bin_path=self.leech_bin,
            db=cdir / "leech" / "catalog.sqlite",
            enc="plain",
            listen=f"127.0.0.1:{lport}",
            tid=ltid,
            log_path=cdir / "log" / "leecher.log",
            env_extra=leech_env or None,
            extra=[
                "--peer",
                f"127.0.0.1:{sport}",
                "--pipeline",
                str(self.args.pipeline),
            ],
        )
        leech_payload = cdir / "leech" / "data" / f"{self.name}.bin"
        ok = wait_complete_log(
            cdir / "log" / "leecher.log",
            cell_timeout,
            self.bytes,
            leech_payload,
        )
        elapsed = time.monotonic() - t0
        if not ok:
            self.cell_fail(f"{cell} (timeout/incomplete after {elapsed:.1f}s)")
            tail_file(cdir / "log" / "leecher.log", 30)
            self.reg.cleanup()
            return
        got = sha256_file(leech_payload)
        if got != self.expect_sha:
            self.cell_fail(f"{cell} (sha mismatch)")
            self.reg.cleanup()
            return
        if elapsed < min_secs:
            self.cell_fail(
                f"{cell} (too fast {elapsed:.1f}s < {min_secs:.1f}s — limit not applied?)"
            )
            self.reg.cleanup()
            return
        note = []
        if upload_bps is not None:
            note.append(f"up={upload_bps}B/s")
        if download_bps is not None:
            note.append(f"down={download_bps}B/s")
        self.cell_pass(f"{cell} ({elapsed:.1f}s; {', '.join(note)})")
        self.reg.cleanup()

    def rate_limit_cells(self) -> None:
        """Upload + download caps at default 2M only (~10s each)."""
        default_2m = parse_size_bytes("2M")
        if self.bytes != default_2m:
            self.cell_skip(
                f"rate-limit (only for default 2M payload; this run is {self.size})"
            )
            return
        # size/rate − burst ≈ 10s with ~1.5s burst → rate ≈ size/11.5
        bps = max(1, int(self.bytes / 11.5))
        print("\n==> rate limits sc→sc (2M, ~10s target)")
        self.sc_sc_rate_limit("rate-limit-upload", upload_bps=bps, target_secs=10.0)
        self.sc_sc_rate_limit("rate-limit-download", download_bps=bps, target_secs=10.0)

    def sc_sc_leech_cache(self, cell: str = "leech-cache") -> None:
        """Leecher stages on paths.leech_cache=/tmp, handoff to permanent data_root."""
        cdir = self.work / "cells" / cell
        self.reg.cleanup()
        if cdir.exists():
            shutil.rmtree(cdir)
        (cdir / "seed" / "data").mkdir(parents=True)
        (cdir / "leech" / "data").mkdir(parents=True)
        (cdir / "log").mkdir(parents=True)

        leech_cache = Path("/tmp")
        sport = self.alloc_port()
        lport = self.alloc_port()
        hardlink_or_copy(self.payload, cdir / "seed" / "data" / f"{self.name}.bin")
        stid = sc_add(
            self.seed_bin,
            cdir / "seed" / "catalog.sqlite",
            self.torrent,
            cdir / "seed" / "data",
        )
        sc_recheck(self.seed_bin, cdir / "seed" / "catalog.sqlite", stid)
        leech_db = cdir / "leech" / "catalog.sqlite"
        leech_home = cdir / "leech" / "data"
        ltid = sc_add(
            self.leech_bin,
            leech_db,
            self.torrent,
            leech_home,
            leech_cache=leech_cache,
        )

        # Placement must have staged under /tmp/{infohash}/ with home_root set.
        try:
            row = sqlite3.connect(leech_db).execute(
                "SELECT data_root, home_root FROM meta_path WHERE torrent_id = ?",
                (ltid,),
            ).fetchone()
        except sqlite3.Error as e:
            self.cell_fail(f"{cell} (catalog: {e})")
            self.reg.cleanup()
            return
        if not row or not row[1]:
            self.cell_fail(f"{cell} (not staged on leech_cache; free space?)")
            self.reg.cleanup()
            return
        stage_root = Path(row[0])
        home_root = Path(row[1])
        if not str(stage_root).startswith(str(leech_cache)):
            self.cell_fail(f"{cell} (stage not under {leech_cache}: {stage_root})")
            self.reg.cleanup()
            return
        if home_root.resolve() != leech_home.resolve():
            self.cell_fail(f"{cell} (home_root mismatch: {home_root})")
            self.reg.cleanup()
            return

        sp = start_sc_swarm(
            self.reg,
            bin_path=self.seed_bin,
            db=cdir / "seed" / "catalog.sqlite",
            enc="plain",
            listen=f"127.0.0.1:{sport}",
            tid=stid,
            log_path=cdir / "log" / "seeder.log",
        )
        if not wait_listen("127.0.0.1", sport, sp, 20):
            self.cell_fail(f"{cell} (seeder listen)")
            tail_file(cdir / "log" / "seeder.log", 20)
            self.reg.cleanup()
            return

        start_sc_swarm(
            self.reg,
            bin_path=self.leech_bin,
            db=leech_db,
            enc="plain",
            listen=f"127.0.0.1:{lport}",
            tid=ltid,
            log_path=cdir / "log" / "leecher.log",
            extra=[
                "--peer",
                f"127.0.0.1:{sport}",
                "--pipeline",
                str(self.args.pipeline),
            ],
        )
        leech_payload = leech_home / f"{self.name}.bin"
        # Handoff copy after complete; allow a little extra wall time.
        mto = self.timeout + 30
        if wait_complete_log(
            cdir / "log" / "leecher.log",
            mto,
            self.bytes,
            leech_payload,
        ):
            try:
                after = sqlite3.connect(leech_db).execute(
                    "SELECT data_root, home_root FROM meta_path WHERE torrent_id = ?",
                    (ltid,),
                ).fetchone()
            except sqlite3.Error as e:
                self.cell_fail(f"{cell} (catalog after: {e})")
                self.reg.cleanup()
                return
            home_ok = (
                after
                and (after[1] is None or after[1] == "")
                and Path(after[0]).resolve() == leech_home.resolve()
            )
            stage_gone = not stage_root.exists()
            sha_ok = sha256_file(leech_payload) == self.expect_sha
            if sha_ok and home_ok and stage_gone:
                self.cell_pass(f"{cell} (handoff /tmp → home)")
            elif not sha_ok:
                self.cell_fail(f"{cell} (sha mismatch at home)")
            elif not home_ok:
                self.cell_fail(f"{cell} (catalog still staged: {after})")
            else:
                self.cell_fail(f"{cell} (stage tree remains: {stage_root})")
        else:
            self.cell_fail(f"{cell} (timeout/incomplete/no handoff to home)")
            tail_file(cdir / "log" / "leecher.log", 40)
        # Best-effort cleanup of leftover stage on failure.
        if stage_root.exists() and stage_root != leech_home:
            shutil.rmtree(stage_root, ignore_errors=True)
        self.reg.cleanup()

    def multi_n2one(self) -> None:
        n = self.args.seeders
        if n <= 0:
            return
        print(f"\n==> multipeer {n} seeders → 1 leecher (plain)")
        cell = "multi-n2one"
        cdir = self.work / "cells" / cell
        self.reg.cleanup()
        if cdir.exists():
            shutil.rmtree(cdir)
        (cdir / "leech" / "data").mkdir(parents=True)
        (cdir / "log").mkdir(parents=True)

        peer_args: list[str] = []
        ok = True
        for i in range(1, n + 1):
            sdir = cdir / f"seed-{i}"
            (sdir / "data").mkdir(parents=True)
            hardlink_or_copy(self.payload, sdir / "data" / f"{self.name}.bin")
            sport = self.alloc_port()
            stid = sc_add(self.seed_bin, sdir / "catalog.sqlite", self.torrent, sdir / "data")
            sc_recheck(self.seed_bin, sdir / "catalog.sqlite", stid)
            sp = start_sc_swarm(
                self.reg,
                bin_path=self.seed_bin,
                db=sdir / "catalog.sqlite",
                enc="plain",
                listen=f"127.0.0.1:{sport}",
                tid=stid,
                log_path=cdir / "log" / f"seeder-{i}.log",
            )
            if not wait_listen("127.0.0.1", sport, sp, 20):
                self.cell_fail(f"{cell} (seeder {i} listen)")
                ok = False
                break
            peer_args.extend(["--peer", f"127.0.0.1:{sport}"])

        if ok and peer_args:
            lport = self.alloc_port()
            ltid = sc_add(
                self.leech_bin,
                cdir / "leech" / "catalog.sqlite",
                self.torrent,
                cdir / "leech" / "data",
            )
            mto = self.timeout + n * 10
            start_sc_swarm(
                self.reg,
                bin_path=self.leech_bin,
                db=cdir / "leech" / "catalog.sqlite",
                enc="plain",
                listen=f"127.0.0.1:{lport}",
                tid=ltid,
                log_path=cdir / "log" / "leecher.log",
                extra=[*peer_args, "--pipeline", str(self.args.pipeline)],
            )
            leech_payload = cdir / "leech" / "data" / f"{self.name}.bin"
            if wait_complete_log(cdir / "log" / "leecher.log", mto, self.bytes, leech_payload):
                if sha256_file(leech_payload) == self.expect_sha:
                    self.cell_pass(cell)
                else:
                    self.cell_fail(f"{cell} (sha)")
            else:
                self.cell_fail(f"{cell} (timeout)")
                tail_file(cdir / "log" / "leecher.log", 30)
        self.reg.cleanup()

    def multi_one2n(self) -> None:
        n = self.args.leechers
        if n <= 0:
            return
        print(f"\n==> multipeer 1 seeder → {n} leechers (plain)")
        cell = "multi-one2n"
        cdir = self.work / "cells" / cell
        self.reg.cleanup()
        if cdir.exists():
            shutil.rmtree(cdir)
        (cdir / "seed" / "data").mkdir(parents=True)
        (cdir / "log").mkdir(parents=True)
        hardlink_or_copy(self.payload, cdir / "seed" / "data" / f"{self.name}.bin")
        sport = self.alloc_port()
        stid = sc_add(
            self.seed_bin,
            cdir / "seed" / "catalog.sqlite",
            self.torrent,
            cdir / "seed" / "data",
        )
        sc_recheck(self.seed_bin, cdir / "seed" / "catalog.sqlite", stid)
        sp = start_sc_swarm(
            self.reg,
            bin_path=self.seed_bin,
            db=cdir / "seed" / "catalog.sqlite",
            enc="plain",
            listen=f"127.0.0.1:{sport}",
            tid=stid,
            log_path=cdir / "log" / "seeder.log",
        )
        if not wait_listen("127.0.0.1", sport, sp, 20):
            self.cell_fail(f"{cell} (seeder listen)")
            self.reg.cleanup()
            return

        mto = self.timeout + n * 15
        payloads: list[Path] = []
        for i in range(1, n + 1):
            ldir = cdir / f"leech-{i}"
            (ldir / "data").mkdir(parents=True)
            lport = self.alloc_port()
            ltid = sc_add(self.leech_bin, ldir / "catalog.sqlite", self.torrent, ldir / "data")
            start_sc_swarm(
                self.reg,
                bin_path=self.leech_bin,
                db=ldir / "catalog.sqlite",
                enc="plain",
                listen=f"127.0.0.1:{lport}",
                tid=ltid,
                log_path=cdir / "log" / f"leecher-{i}.log",
                extra=[
                    "--peer",
                    f"127.0.0.1:{sport}",
                    "--pipeline",
                    str(self.args.pipeline),
                ],
            )
            payloads.append(ldir / "data" / f"{self.name}.bin")

        ok_all = True
        for i, lp in enumerate(payloads, 1):
            logp = cdir / "log" / f"leecher-{i}.log"
            if not wait_complete_log(logp, mto, self.bytes, lp):
                ok_all = False
                print(f"    leecher {i} incomplete", file=sys.stderr)
                tail_file(logp, 15)
            elif sha256_file(lp) != self.expect_sha:
                ok_all = False
                print(f"    leecher {i} sha mismatch", file=sys.stderr)
        if ok_all:
            self.cell_pass(cell)
        else:
            self.cell_fail(cell)
        self.reg.cleanup()

    def rasterbar_cells(self) -> None:
        rb = self.args.rasterbar
        if rb == "off":
            self.cell_skip("rasterbar (--no-rasterbar)")
            return
        if not have_libtorrent_py():
            if rb == "on":
                print("  FAIL rasterbar required but import libtorrent failed", file=sys.stderr)
                self.fail_n += 1
            else:
                self.cell_skip("rasterbar (python libtorrent not installed)")
            return

        print("\n==> rasterbar interop (python libtorrent)")
        for role in self.lt_roles:
            for enc in self.lt_modes:
                self._lt_cell(role, enc)

    def _lt_cell(self, role: str, enc: str) -> None:
        # role is already lt-sc | sc-lt; do not prefix another "lt-"
        cell = f"{role}-{enc}"
        cdir = self.work / "cells" / cell
        self.reg.cleanup()
        if cdir.exists():
            shutil.rmtree(cdir)
        (cdir / "seed" / "data").mkdir(parents=True)
        (cdir / "leech" / "data").mkdir(parents=True)
        (cdir / "log").mkdir(parents=True)

        if role == "lt-sc":
            sport = self.alloc_port()
            lport = self.alloc_port()
            hardlink_or_copy(self.payload, cdir / "seed" / "data" / f"{self.name}.bin")
            self.reg.start(
                [
                    sys.executable,
                    str(LT_PEER),
                    "seed",
                    "--torrent",
                    str(self.torrent),
                    "--data-dir",
                    str(cdir / "seed" / "data"),
                    "--payload",
                    str(self.payload),
                    "--listen",
                    f"127.0.0.1:{sport}",
                    "--encryption",
                    enc,
                ],
                log_path=cdir / "log" / "lt-seeder.log",
            )
            ready = wait_log_contains(
                cdir / "log" / "lt-seeder.log", "lt_peer seed: ready", 10.0
            ) or port_listening("127.0.0.1", sport)
            # extra settle for listen
            if not ready:
                for _ in range(50):
                    if port_listening("127.0.0.1", sport):
                        ready = True
                        break
                    time.sleep(0.1)
            if not ready:
                self.cell_fail(f"{cell} (lt seeder start)")
                tail_file(cdir / "log" / "lt-seeder.log", 20)
                self.reg.cleanup()
                return
            ltid = sc_add(
                self.leech_bin,
                cdir / "leech" / "catalog.sqlite",
                self.torrent,
                cdir / "leech" / "data",
            )
            start_sc_swarm(
                self.reg,
                bin_path=self.leech_bin,
                db=cdir / "leech" / "catalog.sqlite",
                enc=enc,
                listen=f"127.0.0.1:{lport}",
                tid=ltid,
                log_path=cdir / "log" / "sc-leecher.log",
                extra=[
                    "--peer",
                    f"127.0.0.1:{sport}",
                    "--pipeline",
                    str(self.args.pipeline),
                ],
            )
            lp = cdir / "leech" / "data" / f"{self.name}.bin"
            if wait_complete_log(cdir / "log" / "sc-leecher.log", self.timeout, self.bytes, lp):
                if sha256_file(lp) == self.expect_sha:
                    self.cell_pass(cell)
                else:
                    self.cell_fail(f"{cell} (sha)")
            else:
                self.cell_fail(f"{cell} (timeout)")
                tail_file(cdir / "log" / "sc-leecher.log", 25)
                tail_file(cdir / "log" / "lt-seeder.log", 15)

        elif role == "sc-lt":
            sport = self.alloc_port()
            lport = self.alloc_port()
            hardlink_or_copy(self.payload, cdir / "seed" / "data" / f"{self.name}.bin")
            stid = sc_add(
                self.seed_bin,
                cdir / "seed" / "catalog.sqlite",
                self.torrent,
                cdir / "seed" / "data",
            )
            sc_recheck(self.seed_bin, cdir / "seed" / "catalog.sqlite", stid)
            sp = start_sc_swarm(
                self.reg,
                bin_path=self.seed_bin,
                db=cdir / "seed" / "catalog.sqlite",
                enc=enc,
                listen=f"127.0.0.1:{sport}",
                tid=stid,
                log_path=cdir / "log" / "sc-seeder.log",
            )
            if not wait_listen("127.0.0.1", sport, sp, 20):
                self.cell_fail(f"{cell} (sc seeder listen)")
                self.reg.cleanup()
                return
            lt_proc = self.reg.start(
                [
                    sys.executable,
                    str(LT_PEER),
                    "leech",
                    "--torrent",
                    str(self.torrent),
                    "--data-dir",
                    str(cdir / "leech" / "data"),
                    "--listen",
                    f"127.0.0.1:{lport}",
                    "--peer",
                    f"127.0.0.1:{sport}",
                    "--encryption",
                    enc,
                    "--timeout",
                    str(int(self.timeout)),
                ],
                log_path=cdir / "log" / "lt-leecher.log",
            )
            try:
                lt_proc.wait(timeout=self.timeout + 30)
            except Exception:
                pass
            lp = cdir / "leech" / "data" / f"{self.name}.bin"
            ok = False
            if lp.is_file() and file_size(lp) == self.bytes:
                ok = sha256_file(lp) == self.expect_sha
            elif wait_log_contains(
                cdir / "log" / "lt-leecher.log", "lt_peer leech: complete", 0.5
            ):
                try:
                    ok = sha256_file(lp) == self.expect_sha
                except OSError:
                    ok = False
            if ok:
                self.cell_pass(cell)
            else:
                self.cell_fail(f"{cell} (lt leech incomplete)")
                tail_file(cdir / "log" / "lt-leecher.log", 30)
                tail_file(cdir / "log" / "sc-seeder.log", 15)
        else:
            self.cell_fail(f"{cell} (unknown role)")
        self.reg.cleanup()

    def run(self) -> int:
        try:
            self.seed_bin, self.leech_bin = resolve_bins(
                want_build=self.args.build or self.args.debug,
                debug=self.args.debug,
                bin_path=self.args.bin,
                seed_bin=self.args.seed_bin,
                leech_bin=self.args.leech_bin,
            )
        except BenchError as e:
            print(f"error: {e}", file=sys.stderr)
            return 2

        print("==> seedchamp bench smoke")
        print(
            f"    size={self.size} piece={self.piece} modes={{{' '.join(self.modes)}}} "
            f"seeders={self.args.seeders} leechers={self.args.leechers}"
        )
        print(
            f"    disk_backends={self.args.backends} "
            f"upload_backends={self.args.upload_backends} "
            f"timeout={self.timeout:.0f}s rasterbar={self.args.rasterbar}"
        )
        bins_banner(self.seed_bin, self.leech_bin)

        if self.work.exists():
            shutil.rmtree(self.work)
        (self.work / "payload").mkdir(parents=True)
        (self.work / "torrents").mkdir(parents=True)
        (self.work / "logs").mkdir(parents=True)
        (self.work / "cells").mkdir(parents=True)

        self.payload, self.torrent = gen_seed_payload(
            self.name,
            self.size,
            self.piece,
            self.work / "payload",
            self.work / "torrents",
        )
        self.expect_sha = sha256_file(self.payload)
        print(f"    payload sha256={self.expect_sha} bytes={self.bytes}")

        try:
            print("\n==> crypto sc→sc (1 peer)")
            for enc in self.modes:
                self.sc_sc_transfer(f"crypto-{enc}", enc)

            print("\n==> leech_cache sc→sc (stage /tmp → handoff home)")
            self.sc_sc_leech_cache()

            self.multi_n2one()
            self.multi_one2n()

            print("\n==> disk backend smoke")
            for be in resolve_backend_list(self.args.backends):
                self.sc_sc_transfer(f"backend-{be}", "plain", backend=be)

            upload_bes = [
                b
                for b in parse_list_arg(self.args.upload_backends)
                if b and b.lower() not in ("none", "off", "skip")
            ]
            if upload_bes:
                print("\n==> upload backend smoke")
                for ube in upload_bes:
                    self.sc_sc_transfer(
                        f"upload-{ube}", "plain", upload_backend=ube
                    )

            self.rate_limit_cells()

            self.rasterbar_cells()
        finally:
            self.reg.cleanup()

        print(f"\n==> summary pass={self.pass_n} fail={self.fail_n} skip={self.skip_n}")
        if not self.args.keep_work and self.fail_n == 0 and self.work.exists():
            shutil.rmtree(self.work)
        if self.fail_n > 0:
            return 1
        print("==> smoke OK")
        return 0


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        return Smoke(args).run()
    except BenchError as e:
        print(f"error: {e}", file=sys.stderr)
        return 2
    except KeyboardInterrupt:
        print("interrupted", file=sys.stderr)
        return 130


if __name__ == "__main__":
    raise SystemExit(main())
