#!/usr/bin/env python3
"""Minimal libtorrent-rasterbar seeder/leecher for seedchamp/bench interop.

Trackerless: seed listens; leech uses connect_peer(host:port).
Encryption modes match smoke names: plain | handshake | rc4.

Requires: python3 libtorrent bindings (import libtorrent).
"""

from __future__ import annotations

import argparse
import hashlib
import os
import shutil
import sys
import time
from pathlib import Path


def die(msg: str, code: int = 1) -> None:
    print(f"lt_peer: {msg}", file=sys.stderr)
    raise SystemExit(code)


def import_lt():
    try:
        import libtorrent as lt  # type: ignore
    except ImportError as e:
        die(f"import libtorrent failed: {e} (install python3-libtorrent / py-libtorrent-rasterbar)")
    return lt


def apply_bench_session_settings(settings: dict, lt) -> None:
    """Localhost interop: no DHT/LSD, unlimited rates, aggressive unchoke.

    Stock libtorrent defaults (long unchoke intervals, modest send buffers) make
    *lt as seeder* feel slow against a single seedchamp leecher on loopback.
    """
    settings.update(
        {
            "enable_dht": False,
            "enable_lsd": False,
            "enable_upnp": False,
            "enable_natpmp": False,
            "announce_to_all_tiers": False,
            "announce_to_all_trackers": False,
            "enable_outgoing_utp": False,
            "enable_incoming_utp": False,
            "enable_outgoing_tcp": True,
            "enable_incoming_tcp": True,
            # unlimited
            "upload_rate_limit": 0,
            "download_rate_limit": 0,
            "connections_limit": 50,
            "unchoke_slots_limit": 50,
            "connection_speed": 200,
            "allow_multiple_connections_per_ip": True,
            "close_redundant_connections": False,
            # unchoke quickly (defaults are ~15s / ~30s)
            "unchoke_interval": 1,
            "optimistic_unchoke_interval": 1,
            "auto_manage_interval": 1,
            "active_seeds": 10,
            "active_downloads": 10,
            "active_checking": 2,
            "active_limit": 20,
            "auto_manage_startup": 1,
            # request / send pipeline for small localhost transfers
            "max_out_request_queue": 500,
            "max_allowed_in_request_queue": 2000,
            "send_buffer_low_watermark": 512 * 1024,
            "send_buffer_watermark": 4 * 1024 * 1024,
            "send_buffer_watermark_factor": 150,
            "seeding_piece_quota": 50,
            "request_timeout": 10,
            "piece_timeout": 10,
            "peer_timeout": 20,
            "inactivity_timeout": 20,
            "strict_end_game_mode": False,
            "smooth_connects": False,
        }
    )
    # Prefer round-robin seed unchoke so a lone leecher is not starved.
    try:
        settings["seed_choking_algorithm"] = int(lt.seed_choking_algorithm_t.round_robin)
    except Exception:
        pass
    try:
        settings["choking_algorithm"] = int(lt.choking_algorithm_t.fixed_slots_choker)
    except Exception:
        pass


def apply_encryption(settings: dict, mode: str, lt) -> None:
    """Map smoke modes onto rasterbar session settings."""
    apply_bench_session_settings(settings, lt)
    pe_disabled = int(lt.enc_policy.pe_disabled)
    pe_enabled = int(lt.enc_policy.pe_enabled)
    pe_forced = int(lt.enc_policy.pe_forced)
    pe_rc4 = int(lt.enc_level.pe_rc4)
    pe_both = int(lt.enc_level.pe_both)

    if mode == "plain":
        settings["out_enc_policy"] = pe_disabled
        settings["in_enc_policy"] = pe_disabled
        settings["allowed_enc_level"] = pe_both
        settings["prefer_rc4"] = False
    elif mode == "handshake":
        # PE handshake, prefer plaintext crypto
        settings["out_enc_policy"] = pe_enabled
        settings["in_enc_policy"] = pe_enabled
        settings["allowed_enc_level"] = pe_both
        settings["prefer_rc4"] = False
    elif mode == "rc4":
        settings["out_enc_policy"] = pe_forced
        settings["in_enc_policy"] = pe_forced
        settings["allowed_enc_level"] = pe_rc4
        settings["prefer_rc4"] = True
    else:
        die(f"unknown --encryption mode {mode!r} (plain|handshake|rc4)")


def parse_listen(listen: str) -> tuple[str, int]:
    if listen.startswith("["):
        # [ipv6]:port — not needed for localhost smoke
        die(f"unsupported listen {listen!r}")
    if ":" not in listen:
        die(f"listen must be host:port, got {listen!r}")
    host, _, port_s = listen.rpartition(":")
    return host, int(port_s)


def parse_peer(peer: str) -> tuple[str, int]:
    return parse_listen(peer)


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def file_size(path: Path) -> int:
    return path.stat().st_size


def prepare_seed_data(torrent: Path, data_dir: Path, payload_name: str, source_payload: Path | None) -> Path:
    """Ensure data_dir/payload_name exists for seeding (hardlink or copy)."""
    data_dir.mkdir(parents=True, exist_ok=True)
    dest = data_dir / payload_name
    if source_payload is not None:
        if dest.exists() or dest.is_symlink():
            dest.unlink()
        try:
            os.link(source_payload, dest)
        except OSError:
            shutil.copy2(source_payload, dest)
    if not dest.is_file():
        die(f"seed payload missing: {dest}")
    return dest


def torrent_name(lt, torrent: Path) -> str:
    info = lt.torrent_info(str(torrent))
    return info.name()


def make_session(lt, listen: str, mode: str):
    host, port = parse_listen(listen)
    settings = {
        "listen_interfaces": f"{host}:{port}",
        "alert_mask": lt.alert.category_t.status_notification
        | lt.alert.category_t.error_notification,
    }
    apply_encryption(settings, mode, lt)
    return lt.session(settings)


def add_torrent(lt, ses, torrent: Path, data_dir: Path, seed_mode: bool):
    data_dir.mkdir(parents=True, exist_ok=True)
    # libtorrent 2.x: load_torrent_file if present
    if hasattr(lt, "load_torrent_file"):
        atp = lt.load_torrent_file(str(torrent))
    else:
        atp = lt.add_torrent_params()
        atp.ti = lt.torrent_info(str(torrent))
    atp.save_path = str(data_dir)
    if seed_mode and hasattr(lt, "torrent_flags"):
        # Assume complete data on disk — do NOT force_recheck (that undoes this).
        atp.flags |= lt.torrent_flags.seed_mode
        # Stay out of auto-manager queues that can delay seeding.
        try:
            atp.flags &= ~lt.torrent_flags.auto_managed
        except Exception:
            pass
        try:
            atp.flags &= ~lt.torrent_flags.paused
        except Exception:
            pass
    elif seed_mode and hasattr(lt, "add_torrent_params_flags_t"):
        atp.flags |= lt.add_torrent_params_flags_t.flag_seed_mode
    h = ses.add_torrent(atp)
    if seed_mode and hasattr(lt, "torrent_flags"):
        try:
            h.unset_flags(lt.torrent_flags.auto_managed)
            h.unset_flags(lt.torrent_flags.paused)
            h.resume()
        except Exception:
            pass
    return h


def wait_seeding(h, timeout: float) -> bool:
    """Wait until seed_mode/has_metadata shows as seeding without force_recheck."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        s = h.status()
        if getattr(s, "is_seeding", False) or s.progress >= 0.999999:
            return True
        try:
            # checking with seed_mode should flip quickly; do not recheck
            state = int(s.state)
            # seeding / finished enums vary; progress is enough
            if state >= 0 and s.progress >= 0.999:
                return True
        except Exception:
            pass
        time.sleep(0.05)
    return False


def wait_finished(h, timeout: float, label: str) -> None:
    lt = import_lt()
    deadline = time.time() + timeout
    while time.time() < deadline:
        s = h.status()
        if getattr(s, "is_seeding", False) or s.progress >= 0.999999:
            return
        try:
            if s.state in (lt.torrent_status.seeding, lt.torrent_status.finished):
                return
        except Exception:
            pass
        time.sleep(0.05)
    st = h.status()
    err = ""
    if hasattr(st, "errc") and st.errc:
        try:
            err = st.errc.message()
        except Exception:
            err = str(st.errc)
    die(f"{label} timeout progress={st.progress:.4f} state={st.state} err={err}")


def cmd_seed(args) -> int:
    lt = import_lt()
    torrent = Path(args.torrent).resolve()
    data_dir = Path(args.data_dir).resolve()
    name = torrent_name(lt, torrent)
    source = Path(args.payload).resolve() if args.payload else None
    prepare_seed_data(torrent, data_dir, name, source)

    ses = make_session(lt, args.listen, args.encryption)
    h = add_torrent(lt, ses, torrent, data_dir, seed_mode=True)
    # seed_mode: wait for seeding; only recheck as last resort (full hash is slow).
    if not wait_seeding(h, min(15.0, args.timeout)):
        print("lt_peer seed: seed_mode not ready, force_recheck (slow path)", flush=True)
        h.force_recheck()
        if not wait_seeding(h, args.timeout):
            st = h.status()
            die(f"seed not ready progress={st.progress:.4f} state={st.state}")

    host, port = parse_listen(args.listen)
    st = h.status()
    print(
        f"lt_peer seed: ready name={name} listen={host}:{port} "
        f"encryption={args.encryption} progress={st.progress:.4f}",
        flush=True,
    )
    # stay alive until killed; pop alerts often so the session stays responsive
    try:
        while True:
            ses.pop_alerts()
            time.sleep(0.05)
    except KeyboardInterrupt:
        pass
    return 0


def cmd_leech(args) -> int:
    lt = import_lt()
    torrent = Path(args.torrent).resolve()
    data_dir = Path(args.data_dir).resolve()
    if data_dir.exists():
        # clean partial
        for p in data_dir.iterdir():
            if p.is_file():
                p.unlink()
    data_dir.mkdir(parents=True, exist_ok=True)
    name = torrent_name(lt, torrent)

    ses = make_session(lt, args.listen, args.encryption)
    h = add_torrent(lt, ses, torrent, data_dir, seed_mode=False)

    phost, pport = parse_peer(args.peer)
    # connect_peer — trackerless
    ep = (phost, pport)
    try:
        h.connect_peer(ep)
    except TypeError:
        # older API may want lt.address
        h.connect_peer(lt.tcp_endpoint(lt.address(phost), pport))

    print(
        f"lt_peer leech: dialing {phost}:{pport} encryption={args.encryption}",
        flush=True,
    )
    wait_finished(h, args.timeout, "leech")
    payload = data_dir / name
    if not payload.is_file():
        # multi-file would be a directory; single-file only for smoke
        die(f"payload not found after complete: {payload}")
    digest = sha256_file(payload)
    size = file_size(payload)
    print(f"lt_peer leech: complete bytes={size} sha256={digest}", flush=True)
    # keep process briefly so seeder can flush; then exit 0
    return 0


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(description="rasterbar peer for seedchamp/bench")
    p.add_argument(
        "role",
        choices=("seed", "leech"),
        help="seed complete data, or leech via --peer",
    )
    p.add_argument("--torrent", required=True, help="path to .torrent")
    p.add_argument("--data-dir", required=True, help="save / seed data directory")
    p.add_argument(
        "--listen",
        default="127.0.0.1:0",
        help="listen host:port (seed should use fixed port)",
    )
    p.add_argument(
        "--encryption",
        default="plain",
        choices=("plain", "handshake", "rc4"),
        help="smoke encryption mode",
    )
    p.add_argument("--peer", help="host:port of seeder (leech only)")
    p.add_argument(
        "--payload",
        help="source complete payload path to hardlink/copy into data-dir (seed)",
    )
    p.add_argument("--timeout", type=float, default=120.0, help="leech timeout seconds")
    args = p.parse_args(argv)

    if args.role == "seed":
        return cmd_seed(args)
    if not args.peer:
        die("leech requires --peer host:port")
    return cmd_leech(args)


if __name__ == "__main__":
    raise SystemExit(main())
