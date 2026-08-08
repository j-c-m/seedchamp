#!/usr/bin/env python3
"""Generate deterministic seed payload + .torrent for seedchamp/bench.

Examples:
  ./bench/gen_seed.py --name smoke-seed --size 2M --piece-length 32K \
      --data-dir ./bench/work/seed-data --torrent-dir ./bench/work/torrents --force
"""

from __future__ import annotations

import argparse
import hashlib
import re
import struct
import sys
from pathlib import Path


def parse_size(s: str) -> int:
    s = s.strip().upper().replace(" ", "")
    m = re.fullmatch(r"(\d+)([KMGT]I?B?)?", s)
    if not m:
        raise ValueError(f"bad size: {s!r} (use e.g. 100K, 500M, 1024)")
    n = int(m.group(1))
    unit = m.group(2) or ""
    mult = {
        "": 1,
        "K": 1024,
        "KB": 1024,
        "KIB": 1024,
        "M": 1024**2,
        "MB": 1024**2,
        "MIB": 1024**2,
        "G": 1024**3,
        "GB": 1024**3,
        "GIB": 1024**3,
        "T": 1024**4,
        "TB": 1024**4,
        "TIB": 1024**4,
    }.get(unit)
    if mult is None:
        raise ValueError(f"bad size unit in {s!r}")
    return n * mult


def bdecode(data: bytes):
    def _dec(i: int):
        if data[i : i + 1] == b"i":
            j = data.index(b"e", i)
            return int(data[i + 1 : j]), j + 1
        if data[i : i + 1] == b"l":
            out, i = [], i + 1
            while data[i : i + 1] != b"e":
                v, i = _dec(i)
                out.append(v)
            return out, i + 1
        if data[i : i + 1] == b"d":
            out, i = {}, i + 1
            while data[i : i + 1] != b"e":
                k, i = _dec(i)
                v, i = _dec(i)
                out[k] = v
            return out, i + 1
        j = data.index(b":", i)
        n = int(data[i:j])
        s = j + 1
        return data[s : s + n], s + n

    v, _ = _dec(0)
    return v


def bencode(obj: object) -> bytes:
    if isinstance(obj, bool):
        raise TypeError("bool")
    if isinstance(obj, int):
        return b"i" + str(obj).encode() + b"e"
    if isinstance(obj, bytes):
        return str(len(obj)).encode() + b":" + obj
    if isinstance(obj, str):
        return bencode(obj.encode())
    if isinstance(obj, list):
        return b"l" + b"".join(bencode(x) for x in obj) + b"e"
    if isinstance(obj, dict):
        items = sorted(
            obj.items(),
            key=lambda kv: kv[0] if isinstance(kv[0], bytes) else str(kv[0]).encode(),
        )
        out = bytearray(b"d")
        for k, v in items:
            if isinstance(k, str):
                k = k.encode()
            out += bencode(k) + bencode(v)
        out += b"e"
        return bytes(out)
    raise TypeError(type(obj))


def fill_deterministic(path: Path, size: int, seed: int, chunk: int = 1024 * 1024) -> None:
    """Write `size` bytes derived from SHA-256(seed || counter) stream."""
    path.parent.mkdir(parents=True, exist_ok=True)
    counter = 0
    written = 0
    with open(path, "wb") as f:
        while written < size:
            block = bytearray()
            while len(block) < chunk and written + len(block) < size:
                h = hashlib.sha256(struct.pack(">QQ", seed, counter)).digest()
                counter += 1
                block.extend(h)
            take = min(len(block), size - written)
            f.write(block[:take])
            written += take


def build_torrent(
    bin_path: Path,
    torrent_path: Path,
    *,
    name: str,
    piece_length: int,
    announce: str,
) -> None:
    data = bin_path.read_bytes()
    length = len(data)
    pieces = bytearray()
    for off in range(0, length, piece_length):
        pieces += hashlib.sha1(data[off : off + piece_length]).digest()
    info = {
        b"length": length,
        b"name": name.encode() if isinstance(name, str) else name,
        b"piece length": piece_length,
        b"pieces": bytes(pieces),
    }
    meta = {
        b"announce": announce.encode() if isinstance(announce, str) else announce,
        b"info": info,
    }
    torrent_path.parent.mkdir(parents=True, exist_ok=True)
    torrent_path.write_bytes(bencode(meta))
    infohash = hashlib.sha1(bencode(info)).hexdigest()
    print(f"  torrent {torrent_path}  pieces={len(pieces) // 20}  infohash={infohash}")


def main(argv: list[str] | None = None) -> int:
    here = Path(__file__).resolve().parent
    p = argparse.ArgumentParser(description="Generate seed bin + torrent for seedchamp/bench")
    p.add_argument("--name", required=True, help="base name (e.g. test-seed, test-seed-big)")
    p.add_argument("--size", required=True, help="payload size, e.g. 100K, 500M")
    p.add_argument(
        "--piece-length",
        default=None,
        help="piece length, e.g. 32K, 1M (default: 32K if size<1M else 1M)",
    )
    p.add_argument(
        "--seed",
        type=int,
        default=0xC0FFEE,
        help="PRNG seed for deterministic contents (default 0xC0FFEE)",
    )
    p.add_argument(
        "--announce",
        default="http://127.0.0.1:9/announce",
        help="announce URL in torrent",
    )
    p.add_argument(
        "--data-dir",
        type=Path,
        default=here / "work" / "seed-data",
        help="directory for .bin",
    )
    p.add_argument(
        "--torrent-dir",
        type=Path,
        default=here / "work" / "torrents",
        help="directory for .torrent",
    )
    p.add_argument("--force", action="store_true", help="overwrite existing files")
    args = p.parse_args(argv)

    try:
        size = parse_size(args.size)
        if size <= 0:
            raise ValueError("size must be > 0")
        if args.piece_length:
            piece_length = parse_size(args.piece_length)
        else:
            piece_length = 32 * 1024 if size < 1024 * 1024 else 1024 * 1024
        if piece_length < 16 * 1024:
            raise ValueError("piece-length too small")
    except ValueError as e:
        print(f"error: {e}", file=sys.stderr)
        return 2

    bin_name = f"{args.name}.bin"
    # torrent file name: test-seed.torrent / test-seed-big.torrent
    torrent_name = f"{args.name}.torrent"
    bin_path = args.data_dir / bin_name
    torrent_path = args.torrent_dir / torrent_name
    # file name inside torrent metainfo
    file_name = bin_name

    need_bin = args.force or not bin_path.is_file() or bin_path.stat().st_size != size
    need_torrent = args.force or not torrent_path.is_file() or need_bin
    if not need_torrent and torrent_path.is_file():
        try:
            meta = bdecode(torrent_path.read_bytes())
            pl = int(meta[b"info"][b"piece length"])
            if pl != piece_length:
                need_torrent = True
        except Exception:
            need_torrent = True

    if not need_bin and not need_torrent:
        print(f"up-to-date: {bin_path} ({size} bytes) + {torrent_path}")
        return 0

    if need_bin:
        print(f"writing {bin_path} ({size} bytes, seed={args.seed:#x})…")
        fill_deterministic(bin_path, size, args.seed)
        print(f"  wrote {bin_path.stat().st_size} bytes")

    if need_torrent:
        print(f"writing {torrent_path} (piece_length={piece_length})…")
        build_torrent(
            bin_path,
            torrent_path,
            name=file_name,
            piece_length=piece_length,
            announce=args.announce,
        )

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
