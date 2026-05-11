#!/usr/bin/env python3
"""forkd guest agent — runs as PID 1, warms state into memory, accepts
commands from the host via TCP on port 8888.

Protocol: each request is one JSON object terminated by '\n'. Response is
one JSON object terminated by '\n'. Multiple requests on one connection
are allowed.

Actions:
  {"action": "ping"}
    → {"pong": true, "numpy_version": "1.26.4", "pid": 1}

  {"action": "exec", "args": ["python3", "-c", "print(1+1)"], "timeout": 10}
    → {"stdout": "2\n", "stderr": "", "exit_code": 0}

  {"action": "eval", "code": "1 + numpy.zeros(3).sum()"}
    → {"result": "1.0", "exit_code": 0}

This file is copied into the rootfs at / by scripts/build-rootfs.sh, then
launched as PID 1 by /forkd-init.sh after the kernel finishes mounting
/proc /sys /dev.
"""

import json
import os
import socket
import subprocess
import sys
import threading
import time
import traceback

# Warm up state. Importing numpy into PID 1's memory is the whole point of
# the parent VM existing — the snapshot captures this and every child
# inherits the imported module via mmap CoW.
import numpy as _np

NUMPY_VERSION = _np.__version__

print(
    f"forkd: numpy {NUMPY_VERSION} imported in PID {os.getpid()} "
    f"({sys.executable})",
    flush=True,
)
print("forkd: parent VM ready for snapshot. children inherit this state.", flush=True)


def _recv_line(conn: socket.socket) -> bytes:
    buf = bytearray()
    while True:
        chunk = conn.recv(4096)
        if not chunk:
            return bytes(buf)
        buf.extend(chunk)
        nl = buf.find(b"\n")
        if nl >= 0:
            return bytes(buf[: nl + 1])


def _send_json(conn: socket.socket, obj) -> None:
    conn.sendall((json.dumps(obj) + "\n").encode())


def handle(conn: socket.socket, addr) -> None:
    try:
        line = _recv_line(conn)
        if not line:
            return
        cmd = json.loads(line)
        action = cmd.get("action")

        if action == "ping":
            _send_json(conn, {"pong": True, "numpy_version": NUMPY_VERSION, "pid": os.getpid()})

        elif action == "exec":
            args = cmd["args"]
            timeout = cmd.get("timeout", 30)
            r = subprocess.run(args, capture_output=True, timeout=timeout)
            _send_json(
                conn,
                {
                    "stdout": r.stdout.decode("utf-8", "replace"),
                    "stderr": r.stderr.decode("utf-8", "replace"),
                    "exit_code": r.returncode,
                },
            )

        elif action == "eval":
            try:
                result = eval(cmd["code"], {"numpy": _np, "np": _np})
                _send_json(conn, {"result": repr(result), "exit_code": 0})
            except Exception as e:
                _send_json(
                    conn,
                    {
                        "error": f"{type(e).__name__}: {e}",
                        "traceback": traceback.format_exc(),
                        "exit_code": 1,
                    },
                )

        else:
            _send_json(conn, {"error": f"unknown action: {action}", "exit_code": 1})

    except Exception as e:
        try:
            _send_json(
                conn,
                {
                    "error": f"{type(e).__name__}: {e}",
                    "traceback": traceback.format_exc(),
                    "exit_code": 1,
                },
            )
        except OSError:
            pass
    finally:
        try:
            conn.close()
        except OSError:
            pass


def serve() -> None:
    # Retry bind — eth0 might not be fully up at startup.
    last_err = None
    for _ in range(30):
        try:
            s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            s.bind(("0.0.0.0", 8888))
            s.listen(128)
            break
        except OSError as e:
            last_err = e
            time.sleep(0.2)
    else:
        print(f"forkd: failed to bind 0.0.0.0:8888 after retries: {last_err}", flush=True)
        sys.exit(1)

    print("forkd: agent listening on 0.0.0.0:8888", flush=True)

    while True:
        try:
            conn, addr = s.accept()
            threading.Thread(target=handle, args=(conn, addr), daemon=True).start()
        except Exception as e:
            print(f"forkd: accept error: {e}", flush=True)
            time.sleep(0.1)


if __name__ == "__main__":
    serve()
