#!/usr/bin/env python3
"""DiskWorker write bench only (no peers / hash / network).

Wraps `seedchamp-engine` example `disk_write_bench`. Expands a small matrix
of backends × paths × depths and prints the binary's result lines.

Examples:
  ./bench/diskworker.py
  ./bench/diskworker.py --backends thread,uring --paths durable,discard --size 512M
  ./bench/diskworker.py --depths 1,32,128 --layout multi --build
"""

from __future__ import annotations

import argparse
import os
import subprocess
import sys
from pathlib import Path

_BENCH = Path(__file__).resolve().parent
if str(_BENCH) not in sys.path:
    sys.path.insert(0, str(_BENCH))

from common import (  # noqa: E402
    BENCH_DIR,
    SEEDCHAMP_DIR,
    BenchError,
    parse_list_arg,
    parse_size_bytes,
    resolve_backend_list,
)


def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(
        description="DiskWorker write throughput (example disk_write_bench)"
    )
    p.add_argument(
        "--backends",
        default="matrix",
        help="matrix | auto | thread,uring,aio (default: matrix = OS-available)",
    )
    p.add_argument(
        "--paths",
        default="durable,discard",
        help="durable,discard (default: both)",
    )
    p.add_argument(
        "--depths",
        default="32",
        help="comma list of in-flight depths (default: 32)",
    )
    p.add_argument("--size", default="256M", help="bytes written per cell (default: 256M)")
    p.add_argument("--piece-length", default="1M", help="piece size (default: 1M)")
    p.add_argument(
        "--layout",
        default="single",
        choices=("single", "multi"),
        help="single file or dual-span every piece (default: single)",
    )
    p.add_argument(
        "--work",
        type=Path,
        default=None,
        help="work directory (default: bench/work/diskworker)",
    )
    p.add_argument("--keep-work", action="store_true", help="leave payload files")
    p.add_argument("--build", action="store_true", help="cargo build --release the example")
    p.add_argument("--debug", action="store_true", help="build/run debug example")
    p.add_argument(
        "--bin",
        default=None,
        help="path to disk_write_bench binary (skip cargo locate)",
    )
    p.add_argument(
        "--dry-run",
        action="store_true",
        help="print cells only, do not run",
    )
    return p


def example_bin_path(*, debug: bool) -> Path:
    profile = "debug" if debug else "release"
    return SEEDCHAMP_DIR / "target" / profile / "examples" / "disk_write_bench"


def maybe_build(*, debug: bool) -> None:
    cmd = [
        "cargo",
        "build",
        "-q",
        "-p",
        "seedchamp-engine",
        "--example",
        "disk_write_bench",
    ]
    if not debug:
        cmd.append("--release")
    print(f"+ {' '.join(cmd)}", flush=True)
    subprocess.run(cmd, cwd=SEEDCHAMP_DIR, check=True)


def resolve_example_bin(args: argparse.Namespace) -> Path:
    if args.bin:
        p = Path(args.bin)
        if not p.is_file() or not os.access(p, os.X_OK):
            raise BenchError(f"not an executable binary: {p}")
        return p
    if args.build or args.debug:
        maybe_build(debug=args.debug)
    p = example_bin_path(debug=args.debug)
    if not p.is_file() or not os.access(p, os.X_OK):
        # Auto-build once if missing.
        maybe_build(debug=args.debug)
        if not p.is_file() or not os.access(p, os.X_OK):
            raise BenchError(
                f"missing {p}; build with: cargo build -p seedchamp-engine "
                f"--example disk_write_bench{' --release' if not args.debug else ''}"
            )
    return p


def parse_depths(spec: str) -> list[int]:
    out: list[int] = []
    for part in parse_list_arg(spec):
        try:
            n = int(part)
        except ValueError as e:
            raise BenchError(f"bad depth {part!r}") from e
        if n < 1:
            raise BenchError(f"depth must be >= 1, got {n}")
        out.append(n)
    if not out:
        raise BenchError("no depths")
    return out


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    # Validate size early (same units as other benches).
    _ = parse_size_bytes(args.size)
    _ = parse_size_bytes(args.piece_length)

    backends = resolve_backend_list(args.backends)
    paths = parse_list_arg(args.paths)
    for p in paths:
        if p not in ("durable", "discard"):
            raise BenchError(f"unknown path mode {p!r} (durable|discard)")
    depths = parse_depths(args.depths)

    work = args.work or (BENCH_DIR / "work" / "diskworker")
    work = work.resolve()
    work.mkdir(parents=True, exist_ok=True)

    cells = [
        (be, path, depth)
        for be in backends
        for path in paths
        for depth in depths
    ]
    print(
        f"diskworker: {len(cells)} cell(s) size={args.size} piece={args.piece_length} "
        f"layout={args.layout} work={work}",
        flush=True,
    )
    for be, path, depth in cells:
        print(f"  cell backend={be} path={path} depth={depth}", flush=True)

    if args.dry_run:
        return 0

    binary = resolve_example_bin(args)
    print(f"binary={binary}", flush=True)

    failed = 0
    for be, path, depth in cells:
        cmd = [
            str(binary),
            "--backend",
            be,
            "--path",
            path,
            "--size",
            args.size,
            "--piece-length",
            args.piece_length,
            "--depth",
            str(depth),
            "--layout",
            args.layout,
            "--work",
            str(work),
        ]
        if args.keep_work:
            cmd.append("--keep-work")
        print(f"+ {' '.join(cmd)}", flush=True)
        r = subprocess.run(cmd, cwd=SEEDCHAMP_DIR)
        if r.returncode != 0:
            print(
                f"FAIL backend={be} path={path} depth={depth} exit={r.returncode}",
                flush=True,
            )
            failed += 1

    if not args.keep_work and work.exists():
        # Example removes cell dirs; drop empty parent if we created it.
        try:
            if not any(work.iterdir()):
                work.rmdir()
        except OSError:
            pass

    if failed:
        print(f"diskworker: {failed}/{len(cells)} failed", flush=True)
        return 1
    print(f"diskworker: {len(cells)} ok", flush=True)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except BenchError as e:
        print(f"error: {e}", file=sys.stderr)
        raise SystemExit(2) from e
    except subprocess.CalledProcessError as e:
        print(f"error: build/run failed: {e}", file=sys.stderr)
        raise SystemExit(1) from e
