#!/usr/bin/env python3
"""seedchamp/bench throughput — timed sc↔sc durable (and optional discard) transfers."""

from __future__ import annotations

import argparse
import shutil
import sys
import time
from pathlib import Path

# Allow `python3 bench/throughput.py` from seedchamp root.
_BENCH = Path(__file__).resolve().parent
if str(_BENCH) not in sys.path:
    sys.path.insert(0, str(_BENCH))

from common import (  # noqa: E402
    BENCH_DIR,
    BenchError,
    PortAllocator,
    ProcessRegistry,
    bins_banner,
    default_piece_for_size,
    file_size,
    gen_seed_payload,
    hardlink_or_copy,
    install_cleanup_handlers,
    median,
    parse_list_arg,
    parse_size_bytes,
    rate_mbps,
    resolve_backend_list,
    resolve_bins,
    sc_add,
    sc_recheck,
    sha256_file,
    start_sc_swarm,
    tail_file,
    wait_complete_log,
    wait_listen,
)


def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(description="Timed sc→sc throughput (MiB/s)")
    p.add_argument("--size", default="100M", help="Payload (default: 100M)")
    p.add_argument("--piece-length", default=None)
    p.add_argument("--iters", type=int, default=3)
    p.add_argument("--warmup", type=int, default=1)
    p.add_argument("--paths", default="durable", help="durable,discard")
    p.add_argument("--backends", default="auto")
    p.add_argument("--pipeline", type=int, default=64)
    p.add_argument("--timeout", type=float, default=None)
    p.add_argument("--port-base", type=int, default=53910)
    p.add_argument("--work", type=Path, default=None)
    p.add_argument("--bin", default=None)
    p.add_argument("--seed-bin", default=None)
    p.add_argument("--leech-bin", default=None)
    p.add_argument("--build", action="store_true")
    p.add_argument("--debug", action="store_true")
    p.add_argument("--keep-work", action="store_true")
    p.add_argument(
        "--upload-backend",
        default=None,
        metavar="MODE",
        help="Seeder --upload-backend: auto|pread|compio (default: config/env)",
    )
    return p


class Throughput:
    def __init__(self, args: argparse.Namespace) -> None:
        self.args = args
        self.size = args.size
        self.piece = args.piece_length or default_piece_for_size(self.size)
        self.bytes = parse_size_bytes(self.size)
        self.timeout = args.timeout
        if self.timeout is None:
            self.timeout = max(120.0, 90.0 + self.bytes / (4 * 1024 * 1024))
        self.paths = parse_list_arg(args.paths)
        self.work: Path = args.work or (BENCH_DIR / "work" / "throughput")
        self.reg = ProcessRegistry()
        install_cleanup_handlers(self.reg)
        self.ports = PortAllocator(args.port_base)
        self.name = "bench-seed"
        self.fail = 0
        self.seed_bin: Path
        self.leech_bin: Path
        self.torrent: Path
        self.payload: Path
        self.expect_sha: str
        self.rates: dict[tuple[str, str], list[float]] = {}

    def alloc_port(self) -> int:
        p = self.ports.alloc()
        self.reg.register_port(p)
        return p

    def one_run(self, backend: str, path: str, label: str) -> bool:
        rdir = self.work / "runs" / f"{backend}-{path}-{label}"
        self.reg.cleanup()
        if rdir.exists():
            shutil.rmtree(rdir)
        (rdir / "seed" / "data").mkdir(parents=True)
        (rdir / "leech" / "data").mkdir(parents=True)
        (rdir / "log").mkdir(parents=True)

        sport = self.alloc_port()
        lport = self.alloc_port()
        hardlink_or_copy(self.payload, rdir / "seed" / "data" / f"{self.name}.bin")
        stid = sc_add(
            self.seed_bin,
            rdir / "seed" / "catalog.sqlite",
            self.torrent,
            rdir / "seed" / "data",
        )
        sc_recheck(self.seed_bin, rdir / "seed" / "catalog.sqlite", stid)
        ltid = sc_add(
            self.leech_bin,
            rdir / "leech" / "catalog.sqlite",
            self.torrent,
            rdir / "leech" / "data",
        )

        discard_extra: list[str] = []
        if path == "discard":
            discard_extra = ["--discard-writes"]
        seed_extra: list[str] = []
        if self.args.upload_backend:
            seed_extra.extend(["--upload-backend", str(self.args.upload_backend)])

        sp = start_sc_swarm(
            self.reg,
            bin_path=self.seed_bin,
            db=rdir / "seed" / "catalog.sqlite",
            enc="plain",
            listen=f"127.0.0.1:{sport}",
            tid=stid,
            log_path=rdir / "log" / "seeder.log",
            backend=backend,
            extra=seed_extra,
        )
        if not wait_listen("127.0.0.1", sport, sp, 30):
            print(
                f"backend={backend} path={path} label={label} status=seeder_died "
                f"seed_bin={self.seed_bin} leech_bin={self.leech_bin}"
            )
            tail_file(rdir / "log" / "seeder.log", 15)
            self.reg.cleanup()
            return False

        t0 = time.time()
        start_sc_swarm(
            self.reg,
            bin_path=self.leech_bin,
            db=rdir / "leech" / "catalog.sqlite",
            enc="plain",
            listen=f"127.0.0.1:{lport}",
            tid=ltid,
            log_path=rdir / "log" / "leecher.log",
            backend=backend,
            extra=[
                "--peer",
                f"127.0.0.1:{sport}",
                "--pipeline",
                str(self.args.pipeline),
                *discard_extra,
            ],
        )

        status = "timeout"
        leech_payload = rdir / "leech" / "data" / f"{self.name}.bin"
        if path == "discard":
            if wait_complete_log(rdir / "log" / "leecher.log", self.timeout):
                status = "ok"
        else:
            if wait_complete_log(
                rdir / "log" / "leecher.log",
                self.timeout,
                self.bytes,
                leech_payload,
            ):
                if sha256_file(leech_payload) == self.expect_sha:
                    status = "ok"
                else:
                    status = "sha_mismatch"

        el = time.time() - t0
        mbps = rate_mbps(self.bytes, el)
        got_bytes = 0
        if leech_payload.is_file():
            try:
                got_bytes = file_size(leech_payload)
            except OSError:
                got_bytes = 0

        print(
            f"backend={backend} path={path} label={label} "
            f"elapsed_s={el:.3f} rate_MBps={mbps:.1f} status={status} "
            f"got_bytes={got_bytes} expect={self.bytes} "
            f"seed_bin={self.seed_bin} leech_bin={self.leech_bin}"
        )
        if status != "ok":
            tail_file(rdir / "log" / "leecher.log", 25)
            self.reg.cleanup()
            return False

        self.rates.setdefault((backend, path), []).append(mbps)
        self.reg.cleanup()
        return True

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

        print("==> seedchamp bench throughput")
        print(
            f"    size={self.size} piece={self.piece} iters={self.args.iters} "
            f"warmup={self.args.warmup} paths={{{' '.join(self.paths)}}} "
            f"backends={self.args.backends}"
        )
        bins_banner(self.seed_bin, self.leech_bin)

        if self.work.exists():
            shutil.rmtree(self.work)
        (self.work / "payload").mkdir(parents=True)
        (self.work / "torrents").mkdir(parents=True)
        (self.work / "runs").mkdir(parents=True)

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
            for be in resolve_backend_list(self.args.backends):
                for path in self.paths:
                    print(f"\n==> backend={be} path={path}")
                    for w in range(1, self.args.warmup + 1):
                        if not self.one_run(be, path, f"warmup-{w}"):
                            self.fail += 1
                    for i in range(1, self.args.iters + 1):
                        if not self.one_run(be, path, f"run-{i}"):
                            self.fail += 1
                    rates = self.rates.get((be, path), [])
                    if rates:
                        med = median(rates)
                        print(f"median backend={be} path={path} rate_MBps={med:.1f} n={len(rates)}")
        finally:
            self.reg.cleanup()

        print()
        if not self.args.keep_work and self.fail == 0 and self.work.exists():
            shutil.rmtree(self.work)
        if self.fail > 0:
            print(f"==> throughput FAIL ({self.fail} failed runs)", file=sys.stderr)
            return 1
        print("==> throughput OK")
        return 0


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        return Throughput(args).run()
    except BenchError as e:
        print(f"error: {e}", file=sys.stderr)
        return 2
    except KeyboardInterrupt:
        print("interrupted", file=sys.stderr)
        return 130


if __name__ == "__main__":
    raise SystemExit(main())
