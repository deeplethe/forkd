#!/usr/bin/env python3
"""
Pause-window echo server — runs on the HOST.

TCP server that reflects each 16-byte client frame verbatim. Also
logs server-side timestamps so we have two independent clocks on
each round-trip:

    {"event": "accept", "peer": str, "t_ms": int}
    {"event": "frame",  "peer": str, "seq": int, "t_recv_ms": int, "t_send_ms": int}
    {"event": "close",  "peer": str, "t_ms": int, "frames": int, "reason": str}

The orchestrator runs this in the background and tail-collects the
log to disk; the analyzer joins it with the agent log on `seq`.

One-connection-at-a-time is fine for the bench (the agent is the
sole client) and keeps the server simple. SO_REUSEADDR so quick
re-runs don't hit `Address in use`.
"""
import argparse
import json
import socket
import struct
import sys
import threading
import time


FRAME_FMT = "!IQ4s"
FRAME_SIZE = struct.calcsize(FRAME_FMT)


def emit(obj: dict) -> None:
    sys.stdout.write(json.dumps(obj, separators=(",", ":")))
    sys.stdout.write("\n")
    sys.stdout.flush()


def now_ms() -> int:
    return int(time.time() * 1000)


def handle(conn: socket.socket, peer: str) -> None:
    conn.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    emit({"event": "accept", "peer": peer, "t_ms": now_ms()})
    frames = 0
    reason = "eof"
    try:
        while True:
            buf = b""
            while len(buf) < FRAME_SIZE:
                chunk = conn.recv(FRAME_SIZE - len(buf))
                if not chunk:
                    reason = "eof"
                    raise StopIteration
                buf += chunk
            t_recv = now_ms()
            seq, t_send_ms, _pad = struct.unpack(FRAME_FMT, buf)
            # Reflect verbatim — the client matches its own
            # outbound timestamp on the way back.
            conn.sendall(buf)
            frames += 1
            emit(
                {
                    "event": "frame",
                    "peer": peer,
                    "seq": seq,
                    "t_recv_ms": t_recv,
                    "t_send_ms": now_ms(),
                }
            )
    except StopIteration:
        pass
    except OSError as e:
        reason = f"oserror: {e}"
    finally:
        try:
            conn.close()
        except OSError:
            pass
        emit({"event": "close", "peer": peer, "t_ms": now_ms(), "frames": frames, "reason": reason})


def serve(host: str, port: int, accept_one: bool) -> int:
    server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    try:
        server.bind((host, port))
    except OSError as e:
        emit({"event": "error", "what": f"bind {host}:{port}: {e}"})
        return 2
    server.listen(4)
    emit({"event": "listen", "host": host, "port": port})

    try:
        while True:
            conn, addr = server.accept()
            peer = f"{addr[0]}:{addr[1]}"
            t = threading.Thread(target=handle, args=(conn, peer), daemon=True)
            t.start()
            if accept_one:
                # Wait for the one client to disconnect, then exit
                # so the orchestrator can collect logs cleanly.
                t.join()
                return 0
    except KeyboardInterrupt:
        return 0
    finally:
        try:
            server.close()
        except OSError:
            pass


def parse_args(argv=None):
    p = argparse.ArgumentParser(description=__doc__.strip().splitlines()[0])
    p.add_argument("--host", default="0.0.0.0", help="Bind host (default 0.0.0.0)")
    p.add_argument("--port", type=int, default=39999, help="Bind port (default 39999)")
    p.add_argument(
        "--accept-one",
        action="store_true",
        help="Serve exactly one client connection then exit (used by the bench harness)",
    )
    return p.parse_args(argv)


def main() -> int:
    args = parse_args()
    return serve(args.host, args.port, args.accept_one)


if __name__ == "__main__":
    sys.exit(main())
