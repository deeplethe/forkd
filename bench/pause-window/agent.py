#!/usr/bin/env python3
"""
Pause-window agent — runs INSIDE the source sandbox.

Opens a TCP connection to the host-side echo server, sends a
timestamped 16-byte frame every PING_INTERVAL_MS, and logs one
JSON line per round-trip to stdout (the orchestrator captures
stdout via `forkd-controller exec`).

The script intentionally has zero third-party dependencies so it
runs on any python:3.12-slim-class rootfs without rebuilding.

JSONL schema (one event per line):

    {"event": "send",    "seq": int, "t_send_ms": int}
    {"event": "recv",    "seq": int, "t_send_ms": int, "t_recv_ms": int, "rtt_ms": int}
    {"event": "timeout", "seq": int, "t_send_ms": int}
    {"event": "error",   "seq": int, "what": str}
    {"event": "start",   "host": str, "port": int, "interval_ms": int, "duration_s": int}
    {"event": "stop",    "sent": int, "recv": int, "timeouts": int, "errors": int}

The `recv` event's `rtt_ms` is what the analyzer plots over time.
Gaps in `recv` events (and matched `timeout` events) are how the
benchmark detects the BRANCH pause window from the app side.
"""
import argparse
import json
import socket
import struct
import sys
import time
from typing import Optional


# Frame layout sent on the wire:
#   [seq: u32 BE][t_send_ms: u64 BE][padding: 4 bytes zero] = 16 bytes
# The echo server reflects the same 16 bytes verbatim; the agent
# parses seq + t_send_ms back out to compute RTT.
FRAME_FMT = "!IQ4s"
FRAME_SIZE = struct.calcsize(FRAME_FMT)
assert FRAME_SIZE == 16, FRAME_SIZE


def emit(obj: dict) -> None:
    """Print a JSON event to stdout, flushed."""
    sys.stdout.write(json.dumps(obj, separators=(",", ":")))
    sys.stdout.write("\n")
    sys.stdout.flush()


def now_ms() -> int:
    """Wall-clock ms since the unix epoch. Used end-to-end so the
    orchestrator can align with daemon-side timestamps."""
    return int(time.time() * 1000)


def run(
    host: str,
    port: int,
    interval_ms: int,
    duration_s: int,
    read_timeout_ms: int,
) -> int:
    emit(
        {
            "event": "start",
            "host": host,
            "port": port,
            "interval_ms": interval_ms,
            "duration_s": duration_s,
            "read_timeout_ms": read_timeout_ms,
        }
    )

    try:
        sock = socket.create_connection((host, port), timeout=5.0)
    except OSError as e:
        emit({"event": "error", "seq": -1, "what": f"connect: {e}"})
        return 2

    # TCP_NODELAY so the agent's measurements aren't smeared by
    # Nagle's algorithm. The frame is 16 bytes — well under MSS,
    # so without TCP_NODELAY the kernel will buffer it.
    sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    sock.settimeout(read_timeout_ms / 1000.0)

    deadline = time.monotonic() + duration_s
    interval = interval_ms / 1000.0

    sent = 0
    recv = 0
    timeouts = 0
    errors = 0
    seq = 0

    while time.monotonic() < deadline:
        seq += 1
        t_send = now_ms()
        frame = struct.pack(FRAME_FMT, seq & 0xFFFFFFFF, t_send, b"\x00\x00\x00\x00")
        try:
            sock.sendall(frame)
            emit({"event": "send", "seq": seq, "t_send_ms": t_send})
            sent += 1
        except OSError as e:
            errors += 1
            emit({"event": "error", "seq": seq, "what": f"send: {e}"})
            break

        # One-shot read of the echo response. We don't pipeline
        # because that would mask the pause-window stall under
        # post-resume burst delivery.
        try:
            buf = b""
            while len(buf) < FRAME_SIZE:
                chunk = sock.recv(FRAME_SIZE - len(buf))
                if not chunk:
                    raise OSError("peer closed")
                buf += chunk
            t_recv = now_ms()
            r_seq, r_send_ms, _pad = struct.unpack(FRAME_FMT, buf)
            if r_seq != seq & 0xFFFFFFFF:
                # Out-of-order — possible if a stalled response
                # arrived in the next slot. Log but don't crash.
                emit(
                    {
                        "event": "error",
                        "seq": seq,
                        "what": f"seq mismatch: got {r_seq}",
                    }
                )
                errors += 1
                continue
            recv += 1
            emit(
                {
                    "event": "recv",
                    "seq": seq,
                    "t_send_ms": r_send_ms,
                    "t_recv_ms": t_recv,
                    "rtt_ms": t_recv - r_send_ms,
                }
            )
        except socket.timeout:
            timeouts += 1
            emit({"event": "timeout", "seq": seq, "t_send_ms": t_send})
            # Important: continue the loop, don't break. The whole
            # point of the bench is to see how the agent fares
            # _through_ a pause.
        except OSError as e:
            errors += 1
            emit({"event": "error", "seq": seq, "what": f"recv: {e}"})
            # Connection-level failure → stop. This is a data point.
            break

        # Pace the loop. Slip is okay; we don't try to catch up.
        time.sleep(interval)

    try:
        sock.close()
    except OSError:
        pass

    emit(
        {
            "event": "stop",
            "sent": sent,
            "recv": recv,
            "timeouts": timeouts,
            "errors": errors,
        }
    )
    return 0


def parse_args(argv: Optional[list] = None) -> argparse.Namespace:
    p = argparse.ArgumentParser(description=__doc__.strip().splitlines()[0])
    p.add_argument("--host", required=True, help="Echo server host (reachable from inside the sandbox)")
    p.add_argument("--port", type=int, required=True, help="Echo server port")
    p.add_argument(
        "--interval-ms",
        type=int,
        default=100,
        help="Delay between sends, ms (default 100)",
    )
    p.add_argument(
        "--duration-s",
        type=int,
        default=60,
        help="Total runtime, seconds (default 60)",
    )
    p.add_argument(
        "--read-timeout-ms",
        type=int,
        default=30000,
        help="Per-frame socket read timeout, ms (default 30000 — patient agent)",
    )
    return p.parse_args(argv)


def main() -> int:
    args = parse_args()
    return run(
        host=args.host,
        port=args.port,
        interval_ms=args.interval_ms,
        duration_s=args.duration_s,
        read_timeout_ms=args.read_timeout_ms,
    )


if __name__ == "__main__":
    sys.exit(main())
