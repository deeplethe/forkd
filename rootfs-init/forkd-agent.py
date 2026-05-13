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

`eval` semantics depend on the recipe. By default the code is evaluated
as a Python expression against the agent's interpreter (numpy is in
scope when available). If /etc/forkd-recipe.env declares
`FORKD_AGENT_LANG=node`, the same action routes to a warm-up subprocess
(launched per `FORKD_WARMUP_CMD`) over a line-JSON bridge — used by the
playwright-browser recipe to evaluate JS against a warmed Chromium.

This file is copied into the rootfs at / by scripts/build-rootfs.sh, then
launched as PID 1 by /forkd-init.sh after the kernel finishes mounting
/proc /sys /dev.
"""

import itertools
import json
import os
import shlex
import socket
import subprocess
import sys
import threading
import time
import traceback

# Optional warm-up: importing numpy into PID 1's memory is the canonical
# demo of "fork from warmed state". If the image doesn't have numpy, we
# still serve the agent — just without that particular warm import.
try:
    import numpy as _np
    NUMPY_VERSION = _np.__version__
except ImportError:
    _np = None
    NUMPY_VERSION = "not-installed"


def _load_recipe_env(path: str = "/etc/forkd-recipe.env") -> dict:
    """Parse a minimal KEY=VALUE env file. Supports quoted values and # comments.

    Recipes drop this file into the rootfs to declare per-recipe agent
    behaviour without code changes to forkd-agent itself. Currently
    consumed keys:

      FORKD_WARMUP_CMD   shell-tokenised command to spawn before serving
      FORKD_AGENT_LANG   "node" routes the `eval` action to the warmup
                         subprocess via a stdin/stdout JSON bridge.
                         Anything else (or absent) keeps the default
                         Python eval path.
    """
    env: dict = {}
    try:
        with open(path) as f:
            for raw in f:
                s = raw.strip()
                if not s or s.startswith("#"):
                    continue
                key, sep, val = s.partition("=")
                if not sep:
                    continue
                key = key.strip()
                val = val.strip()
                if len(val) >= 2 and val[0] == val[-1] and val[0] in ("'", '"'):
                    val = val[1:-1]
                env[key] = val
    except FileNotFoundError:
        pass
    return env


RECIPE_ENV = _load_recipe_env()
# Allow process env vars to override the recipe file. Useful for dev
# smoke tests on the host before baking a real rootfs, and for kernel
# cmdline-injected overrides at boot.
for _override_key in ("FORKD_WARMUP_CMD", "FORKD_AGENT_LANG"):
    if _override_key in os.environ:
        RECIPE_ENV[_override_key] = os.environ[_override_key]
AGENT_LANG = RECIPE_ENV.get("FORKD_AGENT_LANG", "python")

# Warm-up subprocess state. None on default Python recipes.
_warmup_proc: "subprocess.Popen | None" = None
_warmup_lock = threading.Lock()
_warmup_ready = False
_req_id_counter = itertools.count(1)


def _drain_stderr(proc: subprocess.Popen) -> None:
    """Forward warmup subprocess stderr to agent stdout for visibility."""
    assert proc.stderr is not None
    for raw in iter(proc.stderr.readline, b""):
        sys.stdout.buffer.write(b"forkd-warmup: " + raw)
        sys.stdout.flush()


def _start_warmup() -> None:
    """If FORKD_WARMUP_CMD is set, spawn it and wait for the ready handshake.

    The warmup process speaks a line-JSON protocol on stdin/stdout. First
    line on stdout MUST be {"ready": true} once the workload (e.g.
    headless Chromium) has finished initialising; after that, the agent
    can send {"id", "code"} requests and read replies.
    """
    global _warmup_proc, _warmup_ready
    cmd = RECIPE_ENV.get("FORKD_WARMUP_CMD")
    if not cmd:
        return
    print(f"forkd: starting warmup (lang={AGENT_LANG}): {cmd}", flush=True)
    try:
        _warmup_proc = subprocess.Popen(
            shlex.split(cmd),
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            bufsize=0,
        )
    except Exception as e:
        print(f"forkd: failed to spawn warmup: {e}", flush=True)
        return

    threading.Thread(target=_drain_stderr, args=(_warmup_proc,), daemon=True).start()

    # Block on the ready handshake. Warmup may take seconds (Chromium
    # launch, model load, etc.); the snapshot --boot-wait-secs flag
    # gives this room to complete.
    ready_line = _warmup_proc.stdout.readline()
    if not ready_line:
        rc = _warmup_proc.poll()
        print(f"forkd: warmup exited before ready (rc={rc})", flush=True)
        return
    try:
        msg = json.loads(ready_line)
    except Exception as e:
        print(f"forkd: warmup ready parse error: {e}, raw={ready_line!r}", flush=True)
        return
    if msg.get("ready"):
        _warmup_ready = True
        print("forkd: warmup ready", flush=True)
    else:
        print(f"forkd: warmup signalled: {msg}", flush=True)


def _bridge_eval(code: str) -> dict:
    """Route an `eval` action to the warmup subprocess and return the reply.

    Serialised by _warmup_lock so concurrent connections within one VM
    don't interleave on the shared stdin/stdout. Cross-VM concurrency
    is unaffected since each child VM has its own agent + warmup pair.
    """
    if not _warmup_ready or _warmup_proc is None:
        return {"error": "warmup not ready", "exit_code": 1}
    req_id = str(next(_req_id_counter))
    payload = json.dumps({"id": req_id, "code": code}).encode() + b"\n"
    with _warmup_lock:
        try:
            _warmup_proc.stdin.write(payload)
            _warmup_proc.stdin.flush()
            resp_line = _warmup_proc.stdout.readline()
        except (BrokenPipeError, OSError) as e:
            return {"error": f"warmup pipe: {e}", "exit_code": 1}
    if not resp_line:
        return {"error": "warmup closed stdout", "exit_code": 1}
    try:
        resp = json.loads(resp_line)
    except Exception as e:
        return {
            "error": f"bridge parse: {e}",
            "raw": resp_line.decode(errors="replace"),
            "exit_code": 1,
        }
    if "error" in resp:
        return {
            "error": resp["error"],
            "stack": resp.get("stack", ""),
            "exit_code": 1,
        }
    # Distinct field name from the Python eval path's `result` (which is
    # a Python repr() string). `result_json` is a JSON-encoded value; the
    # SDK json.loads it back into a native Python object. This keeps the
    # two eval paths cleanly distinguishable on the wire.
    return {"result_json": json.dumps(resp.get("result")), "exit_code": 0}


print(
    f"forkd: numpy={NUMPY_VERSION} agent starting in PID {os.getpid()} "
    f"({sys.executable})",
    flush=True,
)
_start_warmup()
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
            _send_json(
                conn,
                {
                    "pong": True,
                    "numpy_version": NUMPY_VERSION,
                    "pid": os.getpid(),
                    "agent_lang": AGENT_LANG,
                    "warmup_ready": _warmup_ready,
                },
            )

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
            if AGENT_LANG == "node":
                _send_json(conn, _bridge_eval(cmd["code"]))
            else:
                try:
                    eval_globals = {}
                    if _np is not None:
                        eval_globals["numpy"] = _np
                        eval_globals["np"] = _np
                    result = eval(cmd["code"], eval_globals)
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
