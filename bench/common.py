#!/usr/bin/env python3
"""Shared helpers for seedchamp/bench (smoke + throughput).

Process model:
  - Child seedchamp/lt_peer processes use start_new_session=True (own session/PG).
  - Cleanup only signals those child groups — never our PGID/PPID, never by port.
  - Logs go to files; harness prints short PASS/FAIL lines.
  - atexit + SIGTERM/SIGINT handlers only terminate registered child groups.
"""

from __future__ import annotations

import atexit
import hashlib
import os
import platform
import re
import shutil
import signal
import socket
import subprocess
import sys
import time
from pathlib import Path
from typing import Iterable, Sequence

BENCH_DIR = Path(__file__).resolve().parent
SEEDCHAMP_DIR = BENCH_DIR.parent
GEN_SEED = BENCH_DIR / "gen_seed.py"
LT_PEER = BENCH_DIR / "lt_peer.py"

DEFAULT_PORT_BASE = 53810
COMPLETE_RE = re.compile(r"all target torrents complete")

# Smoke mode name -> seedchamp --encryption value
SC_ENC = {
    "plain": "off",
    "handshake": "prefer-plain",
    "rc4": "require-rc4",
    "off": "off",
    "prefer-plain": "prefer-plain",
    "prefer-rc4": "prefer-rc4",
    "require-rc4": "require-rc4",
}


class BenchError(RuntimeError):
    pass


def sc_enc_flag(mode: str) -> str:
    try:
        return SC_ENC[mode]
    except KeyError as e:
        raise BenchError(f"unknown encryption mode: {mode}") from e


def parse_size_bytes(s: str) -> int:
    s = s.strip().upper().replace(" ", "")
    m = re.fullmatch(r"(\d+)([KMGT]I?B?)?", s)
    if not m:
        raise BenchError(f"bad size {s!r}")
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
        raise BenchError(f"bad unit in {s!r}")
    return n * mult


def default_piece_for_size(size_label: str) -> str:
    # Smoke-sized payloads use small pieces; throughput (≥100M) uses 1M.
    return "32K" if parse_size_bytes(size_label) < 16 * 1024 * 1024 else "1M"


def sha256_file(path: Path | str) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def file_size(path: Path | str) -> int:
    return Path(path).stat().st_size


def rate_mbps(nbytes: int, secs: float) -> float:
    return (nbytes / 1e6) / max(secs, 0.001)


def median(vals: Sequence[float]) -> float:
    if not vals:
        return float("nan")
    s = sorted(vals)
    n = len(s)
    if n % 2:
        return s[n // 2]
    return (s[n // 2 - 1] + s[n // 2]) / 2.0


def hardlink_or_copy(src: Path, dst: Path) -> None:
    dst.parent.mkdir(parents=True, exist_ok=True)
    if dst.exists() or dst.is_symlink():
        dst.unlink()
    try:
        os.link(src, dst)
    except OSError:
        shutil.copy2(src, dst)


def port_listening(host: str, port: int, timeout: float = 0.3) -> bool:
    s = socket.socket()
    s.settimeout(timeout)
    try:
        s.connect((host, port))
    except OSError:
        return False
    finally:
        s.close()
    return True


def available_backends() -> list[str]:
    system = platform.system()
    if system == "Linux":
        return ["thread", "uring"]
    if system == "FreeBSD":
        return ["thread", "aio"]
    if system == "Darwin":
        return ["thread", "aio"]
    return ["thread"]


def resolve_backend_list(spec: str) -> list[str]:
    spec = (spec or "auto").strip()
    if spec in ("auto", ""):
        return [os.environ.get("SEEDCHAMP_DISK_BACKEND", "auto")]
    if spec == "matrix":
        return available_backends()
    return [x.strip() for x in spec.replace(",", " ").split() if x.strip()]


def have_libtorrent_py() -> bool:
    try:
        import libtorrent  # noqa: F401
        return True
    except ImportError:
        return False


def find_default_bin() -> Path | None:
    env = os.environ.get("SEEDCHAMP_BIN")
    if env and os.access(env, os.X_OK):
        return Path(env)
    for rel in ("target/release/seedchamp", "target/debug/seedchamp"):
        p = SEEDCHAMP_DIR / rel
        if p.is_file() and os.access(p, os.X_OK):
            return p
    return None


def maybe_build(debug: bool = False) -> None:
    cmd = ["cargo", "build", "-q"]
    if not debug:
        cmd.append("--release")
    subprocess.run(cmd, cwd=SEEDCHAMP_DIR, check=True)


def resolve_bins(
    *,
    want_build: bool = False,
    debug: bool = False,
    bin_path: str | None = None,
    seed_bin: str | None = None,
    leech_bin: str | None = None,
) -> tuple[Path, Path]:
    if want_build:
        maybe_build(debug=debug)

    default: Path | None
    if bin_path:
        default = Path(bin_path)
    elif os.environ.get("SEEDCHAMP_BIN"):
        default = Path(os.environ["SEEDCHAMP_BIN"])
    else:
        default = find_default_bin()
    if default is None:
        raise BenchError(
            "no seedchamp binary; build with cargo or pass --bin / --seed-bin / --leech-bin"
        )

    seed = Path(
        seed_bin
        or os.environ.get("SEEDCHAMP_SEED_BIN")
        or default
    )
    leech = Path(
        leech_bin
        or os.environ.get("SEEDCHAMP_LEECH_BIN")
        or default
    )
    for label, p in (("seed", seed), ("leech", leech)):
        if not p.is_file() or not os.access(p, os.X_OK):
            raise BenchError(f"{label} binary not executable: {p}")
    return seed, leech


def bin_version(bin_path: Path) -> str:
    try:
        out = subprocess.run(
            [str(bin_path), "version"],
            capture_output=True,
            text=True,
            timeout=10,
            check=False,
        )
        line = (out.stdout or out.stderr or "?").strip().splitlines()
        return line[0] if line else "?"
    except Exception:
        return "?"


def bins_banner(seed: Path, leech: Path) -> None:
    print(f"  seed_bin={seed} ({bin_version(seed)})")
    print(f"  leech_bin={leech} ({bin_version(leech)})")


def gen_seed_payload(
    name: str,
    size: str,
    piece: str,
    data_dir: Path,
    torrent_dir: Path,
) -> tuple[Path, Path]:
    data_dir.mkdir(parents=True, exist_ok=True)
    torrent_dir.mkdir(parents=True, exist_ok=True)
    cmd = [
        sys.executable,
        str(GEN_SEED),
        "--name",
        name,
        "--size",
        size,
        "--piece-length",
        piece,
        "--data-dir",
        str(data_dir),
        "--torrent-dir",
        str(torrent_dir),
        "--force",
    ]
    subprocess.run(cmd, check=True)
    return data_dir / f"{name}.bin", torrent_dir / f"{name}.torrent"


def sc_add(
    bin_path: Path,
    db: Path,
    torrent: Path,
    data_root: Path,
    *,
    leech_cache: Path | str | None = None,
) -> int:
    """Add torrent with `--data-root` as permanent layout.

    By default **disables** `paths.leech_cache` (empty `SEEDCHAMP_LEECH_CACHE`) so
    user config cannot stage under `{cache}/{infohash}/` while the bench payload
    was hardlinked into `data_root` — that mismatch leaves seeder `have=0` and
    hangs crypto cells. Pass `leech_cache=` only for the dedicated leech_cache
    smoke cell.
    """
    db.parent.mkdir(parents=True, exist_ok=True)
    data_root.mkdir(parents=True, exist_ok=True)
    env = os.environ.copy()
    # Empty string overrides config leech_cache (see config env apply).
    env["SEEDCHAMP_LEECH_CACHE"] = (
        str(leech_cache) if leech_cache is not None else ""
    )
    out = subprocess.run(
        [
            str(bin_path),
            "--db",
            str(db),
            "torrent",
            "add",
            str(torrent),
            "--data-root",
            str(data_root),
            "--no-save-torrent",
        ],
        capture_output=True,
        text=True,
        check=False,
        env=env,
    )
    text = (out.stdout or "") + (out.stderr or "")
    if out.returncode != 0:
        raise BenchError(f"add failed: {text.strip()}")
    m = re.search(r"id=(\d+)", text)
    if not m:
        raise BenchError(f"parse add id failed: {text.strip()}")
    return int(m.group(1))


def sc_recheck(bin_path: Path, db: Path, tid: int) -> None:
    """Recheck torrent; fail if the process errors or data is not complete.

    Silent incomplete recheck (e.g. payload not under catalog data_root) used to
    leave seeder `have=0` and hang smoke on the first crypto cell.
    """
    out = subprocess.run(
        [str(bin_path), "--db", str(db), "torrent", "recheck", str(tid)],
        capture_output=True,
        text=True,
        check=False,
    )
    text = (out.stdout or "") + (out.stderr or "")
    if out.returncode != 0:
        raise BenchError(f"recheck id={tid} failed: {text.strip()}")
    # "recheck done: pieces=N good=G bad=B missing=M complete=true|false"
    m = re.search(
        r"complete=(true|false)|\"complete\":\s*(true|false)",
        text,
        re.IGNORECASE,
    )
    if m:
        complete = (m.group(1) or m.group(2) or "").lower() == "true"
        if not complete:
            raise BenchError(
                f"recheck id={tid} not complete (payload missing or wrong path?): {text.strip()}"
            )
    elif "recheck done" not in text and "recheck" not in text.lower():
        raise BenchError(f"recheck id={tid} unexpected output: {text.strip()}")


class ProcessRegistry:
    """Track child processes started in their own sessions for safe cleanup.

    Critical safety rules:
      - Children use start_new_session=True (new session + process group).
      - cleanup() only signals those child process groups (by child PID/PGID).
      - Never kill by TCP port (no fuser/lsof). Never signal our own PGID/PPID.
    """

    def __init__(self) -> None:
        self.procs: list[subprocess.Popen] = []
        self.ports: list[int] = []  # informational only (no kill-by-port)
        self._our_pid = os.getpid()
        try:
            self._our_pgid = os.getpgid(0)
        except OSError:
            self._our_pgid = self._our_pid
        self._cleaning = False

    def register_port(self, port: int) -> None:
        """Record a port for diagnostics only — never used to kill processes."""
        self.ports.append(port)

    def start(
        self,
        cmd: Sequence[str],
        *,
        log_path: Path,
        env: dict[str, str] | None = None,
        cwd: Path | str | None = None,
    ) -> subprocess.Popen:
        log_path.parent.mkdir(parents=True, exist_ok=True)
        logf = open(log_path, "ab", buffering=0)
        # Own session/process group: cleanup never signals our parent (the agent).
        proc = subprocess.Popen(
            list(cmd),
            stdout=logf,
            stderr=subprocess.STDOUT,
            env=env,
            cwd=cwd,
            start_new_session=True,
            close_fds=True,
        )
        # Keep file open for child's lifetime; close on cleanup via proc handle.
        proc._bench_logf = logf  # type: ignore[attr-defined]
        self.procs.append(proc)
        return proc

    def _safe_killpg(self, proc: subprocess.Popen, sig: int) -> None:
        """Signal only the child's process group; never our own group/parent."""
        pid = proc.pid
        if pid is None or pid <= 1:
            return
        if pid == self._our_pid or pid == os.getppid():
            return
        try:
            pgid = os.getpgid(pid)
        except (ProcessLookupError, PermissionError, OSError):
            # Process already gone; try direct terminate as last resort.
            try:
                if proc.poll() is None:
                    proc.send_signal(sig)
            except Exception:
                pass
            return
        if pgid <= 1 or pgid == self._our_pgid or pgid == self._our_pid:
            # Refuse to signal our own process group (would kill the agent shell).
            try:
                if proc.poll() is None and pid != self._our_pid:
                    proc.send_signal(sig)
            except Exception:
                pass
            return
        try:
            os.killpg(pgid, sig)
        except (ProcessLookupError, PermissionError, OSError):
            try:
                if proc.poll() is None:
                    proc.send_signal(sig)
            except Exception:
                pass

    def cleanup(self) -> None:
        if self._cleaning:
            return
        self._cleaning = True
        try:
            # SIGTERM process groups first, then SIGKILL stragglers.
            for proc in self.procs:
                if proc.poll() is not None:
                    continue
                self._safe_killpg(proc, signal.SIGTERM)
            deadline = time.time() + 2.0
            for proc in self.procs:
                remaining = max(0.0, deadline - time.time())
                try:
                    proc.wait(timeout=remaining if remaining > 0 else 0.01)
                except subprocess.TimeoutExpired:
                    self._safe_killpg(proc, signal.SIGKILL)
                    try:
                        proc.wait(timeout=1.0)
                    except Exception:
                        pass
                logf = getattr(proc, "_bench_logf", None)
                if logf is not None:
                    try:
                        logf.close()
                    except Exception:
                        pass
                    try:
                        delattr(proc, "_bench_logf")
                    except Exception:
                        pass
            self.procs.clear()
            self.ports.clear()
        finally:
            self._cleaning = False


# Module-level active registry for atexit / signal handlers (one harness at a time).
_ACTIVE_REG: ProcessRegistry | None = None
_HANDLERS_INSTALLED = False


def install_cleanup_handlers(reg: ProcessRegistry) -> None:
    """Register atexit + SIGTERM/SIGINT so children die without touching the parent session."""
    global _ACTIVE_REG, _HANDLERS_INSTALLED
    _ACTIVE_REG = reg

    def _cleanup_active() -> None:
        active = _ACTIVE_REG
        if active is not None:
            active.cleanup()

    if not _HANDLERS_INSTALLED:
        atexit.register(_cleanup_active)

        def _on_signal(signum: int, _frame: object) -> None:
            _cleanup_active()
            # Re-raise default behavior with a clear exit code.
            raise SystemExit(128 + signum)

        for sig in (signal.SIGTERM, signal.SIGINT):
            try:
                signal.signal(sig, _on_signal)
            except (ValueError, OSError):
                pass
        _HANDLERS_INSTALLED = True


def wait_listen(
    host: str,
    port: int,
    proc: subprocess.Popen | None = None,
    timeout_s: float = 15.0,
) -> bool:
    deadline = time.time() + timeout_s
    while time.time() < deadline:
        if port_listening(host, port):
            return True
        if proc is not None and proc.poll() is not None:
            print(f"process {proc.pid} died before listen on {port}", file=sys.stderr)
            return False
        time.sleep(0.1)
    print(f"timeout waiting for {host}:{port}", file=sys.stderr)
    return False


def wait_complete_log(
    log_path: Path,
    timeout_s: float,
    expect_bytes: int | None = None,
    payload: Path | None = None,
) -> bool:
    deadline = time.time() + timeout_s
    while time.time() < deadline:
        if _complete_ok(log_path, expect_bytes, payload):
            return True
        time.sleep(0.05)
    return _complete_ok(log_path, expect_bytes, payload)


def _complete_ok(
    log_path: Path,
    expect_bytes: int | None,
    payload: Path | None,
) -> bool:
    if not log_path.is_file():
        return False
    try:
        text = log_path.read_text(errors="replace")
    except OSError:
        return False
    if not COMPLETE_RE.search(text):
        return False
    if expect_bytes is not None and payload is not None:
        try:
            return file_size(payload) == expect_bytes
        except OSError:
            return False
    return True


def wait_log_contains(log_path: Path, needle: str, timeout_s: float = 10.0) -> bool:
    deadline = time.time() + timeout_s
    while time.time() < deadline:
        try:
            if log_path.is_file() and needle in log_path.read_text(errors="replace"):
                return True
        except OSError:
            pass
        time.sleep(0.1)
    return False


def tail_file(path: Path, n: int = 25) -> None:
    if not path.is_file():
        return
    try:
        lines = path.read_text(errors="replace").splitlines()
    except OSError:
        return
    for line in lines[-n:]:
        print(line, file=sys.stderr)


def start_sc_swarm(
    reg: ProcessRegistry,
    *,
    bin_path: Path,
    db: Path,
    enc: str,
    listen: str,
    tid: int,
    log_path: Path,
    extra: Iterable[str] = (),
    backend: str | None = None,
    rust_log: str | None = None,
    env_extra: dict[str, str] | None = None,
) -> subprocess.Popen:
    enc_val = sc_enc_flag(enc)
    cmd = [
        str(bin_path),
        "--db",
        str(db),
        "bench",
        "swarm",
        "--encryption",
        enc_val,
        "--torrent",
        str(tid),
        "--listen",
        listen,
        "--no-announce",
        *list(extra),
    ]
    env = os.environ.copy()
    env["RUST_LOG"] = rust_log or env.get("RUST_LOG", "info")
    if backend:
        env["SEEDCHAMP_DISK_BACKEND"] = backend
    if env_extra:
        env.update(env_extra)
    return reg.start(cmd, log_path=log_path, env=env)


class PortAllocator:
    def __init__(self, base: int) -> None:
        self.next = base

    def alloc(self) -> int:
        p = self.next
        self.next += 1
        return p


def parse_list_arg(s: str) -> list[str]:
    return [x.strip() for x in s.replace(",", " ").split() if x.strip()]
