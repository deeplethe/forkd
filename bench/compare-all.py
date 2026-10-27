#!/usr/bin/env python3
"""Unified benchmark — spawn N sandboxes that import numpy, on each backend.

Backends:
  - sandstorm       — sandstorm Python SDK (warmed parent already has numpy)
  - cubesandbox — e2b_code_interpreter (CubeSandbox is E2B-compatible)
  - boxlite     — boxlite Python SDK (each Box is a microVM with its own OCI rootfs)
  - opensandbox — opensandbox Python SDK (Docker runtime by default)
  - docker      — `docker run python:3.12-slim python -c "import numpy"`

Measures total wall-clock to spawn N sandboxes that have all evaluated
`numpy.zeros(5).tolist()`. Output: JSON suitable for chart generation.

Run on the same Linux box for fair comparison.
"""

import argparse
import json
import os
import subprocess
import sys
import time
from concurrent.futures import ThreadPoolExecutor, as_completed


def bench_sandstorm(n: int) -> dict:
    """Spawn N sandstorm children (per-child netns) and eval numpy in each."""
    # Resolve the SDK path relative to this script (bench/ → ../sdk/python)
    # so the bench works on any clone, not just one specific dev box.
    sdk_path = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "sdk", "python")
    sys.path.insert(0, sdk_path)
    from sandstorm import Sandbox

    # sandstorm uses --per-child-netns mode under the hood. We send each child
    # a numpy eval to confirm the warmed state is reachable.
    t0 = time.perf_counter()

    # Spawn N children via a single `sandstorm fork` invocation, then talk to
    # each via its netns. Simpler than N separate Sandbox() instances.
    proc = subprocess.Popen(
        ["sudo", "-E", "sandstorm", "fork", "--tag", "pyagent",
         "-n", str(n), "--settle-secs", str(60), "--per-child-netns"],
        stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
    )

    # Wait until all children are up (probe each)
    import socket
    def probe(i):
        # enter netns is host-side; we use sudo sandstorm ping
        r = subprocess.run(
            ["sudo", "sandstorm", "ping", "--child", f"sandstorm-child-{i}"],
            capture_output=True, timeout=30,
        )
        return r.returncode == 0

    deadline = time.perf_counter() + 60
    alive = 0
    while alive < n and time.perf_counter() < deadline:
        alive = sum(1 for i in range(1, n + 1) if probe(i))
        if alive < n:
            time.sleep(0.2)
    t_spawn = time.perf_counter()

    # eval numpy in each
    def eval_one(i):
        r = subprocess.run(
            ["sudo", "sandstorm", "eval", "--child", f"sandstorm-child-{i}",
             "--", "numpy.zeros(5).tolist()"],
            capture_output=True, timeout=10,
        )
        return r.returncode == 0
    with ThreadPoolExecutor(max_workers=n) as ex:
        results = list(ex.map(eval_one, range(1, n + 1)))
    t_eval = time.perf_counter()

    proc.terminate()
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        proc.kill()

    return {
        "backend": "sandstorm",
        "n": n,
        "spawn_ms": int((t_spawn - t0) * 1000),
        "eval_ms": int((t_eval - t_spawn) * 1000),
        "total_ms": int((t_eval - t0) * 1000),
        "succeeded": sum(results),
    }


def bench_docker(n: int) -> dict:
    """N parallel `docker run python:3.12-slim python -c '...'`."""
    subprocess.run(["docker", "pull", "-q", "python:3.12-slim"],
                   check=True, capture_output=True)

    t0 = time.perf_counter()

    def run_one(i):
        r = subprocess.run(
            ["docker", "run", "--rm", "python:3.12-slim",
             "python", "-c",
             "import numpy; print(numpy.zeros(5).tolist())"],
            capture_output=True, timeout=60,
        )
        return r.returncode == 0

    with ThreadPoolExecutor(max_workers=n) as ex:
        results = list(ex.map(run_one, range(n)))
    t_total = time.perf_counter()

    return {
        "backend": "docker",
        "n": n,
        "spawn_ms": int((t_total - t0) * 1000),
        "eval_ms": 0,  # combined
        "total_ms": int((t_total - t0) * 1000),
        "succeeded": sum(results),
    }


def bench_boxlite(n: int) -> dict:
    """Spawn N BoxLite microVMs in parallel, run `import numpy` in each."""
    try:
        import asyncio
        import boxlite
    except ImportError:
        return {"backend": "boxlite", "n": n,
                "error": "boxlite not installed (pip install boxlite)"}

    async def one(_i):
        try:
            async with boxlite.SimpleBox(image="python:3.12-slim") as box:
                r = await box.exec(
                    "python",
                    "-c",
                    "import numpy; print(numpy.zeros(5).tolist())",
                )
                return getattr(r, "exit_code", 0) == 0
        except Exception:
            return False

    async def run_all():
        return await asyncio.gather(*(one(i) for i in range(n)))

    t0 = time.perf_counter()
    results = asyncio.run(run_all())
    t_total = time.perf_counter()

    return {
        "backend": "boxlite",
        "n": n,
        "spawn_ms": int((t_total - t0) * 1000),
        "eval_ms": 0,
        "total_ms": int((t_total - t0) * 1000),
        "succeeded": sum(1 for r in results if r),
    }


def bench_opensandbox(n: int, image: str) -> dict:
    """Spawn N OpenSandbox sandboxes in parallel via the Python SDK.

    Requires an OpenSandbox server already running locally (default port 8000):
        uvx opensandbox-server
    """
    try:
        import asyncio
        from datetime import timedelta

        from opensandbox import Sandbox
    except ImportError:
        return {"backend": "opensandbox", "n": n,
                "error": "opensandbox not installed (pip install opensandbox)"}

    async def one(_i):
        try:
            sandbox = await Sandbox.create(
                image,
                entrypoint=["/opt/opensandbox/code-interpreter.sh"],
                timeout=timedelta(minutes=2),
            )
            async with sandbox:
                exe = await sandbox.commands.run(
                    "python -c 'import numpy; print(numpy.zeros(5).tolist())'"
                )
                ok = getattr(exe, "exit_code", 0) == 0
            await sandbox.kill()
            return ok
        except Exception:
            return False

    async def run_all():
        return await asyncio.gather(*(one(i) for i in range(n)))

    t0 = time.perf_counter()
    results = asyncio.run(run_all())
    t_total = time.perf_counter()

    return {
        "backend": "opensandbox",
        "n": n,
        "spawn_ms": int((t_total - t0) * 1000),
        "eval_ms": 0,
        "total_ms": int((t_total - t0) * 1000),
        "succeeded": sum(1 for r in results if r),
    }


def bench_cubesandbox(n: int, template_id: str) -> dict:
    """Spawn N CubeSandbox sandboxes via E2B SDK + eval numpy in each."""
    try:
        from e2b_code_interpreter import Sandbox
    except ImportError:
        return {
            "backend": "cubesandbox",
            "n": n,
            "error": "e2b_code_interpreter not installed (pip install e2b-code-interpreter)",
        }

    # CubeSandbox: point at local cubeapi
    os.environ.setdefault("E2B_API_URL", "http://127.0.0.1:9000")
    os.environ.setdefault("E2B_API_KEY", "local-no-auth")

    t0 = time.perf_counter()

    def spawn_one(i):
        try:
            sb = Sandbox.create(template=template_id)
            r = sb.run_code("import numpy; print(numpy.zeros(5).tolist())")
            sb.kill()
            return True
        except Exception:
            return False

    with ThreadPoolExecutor(max_workers=n) as ex:
        results = list(ex.map(spawn_one, range(n)))
    t_total = time.perf_counter()

    return {
        "backend": "cubesandbox",
        "n": n,
        "spawn_ms": int((t_total - t0) * 1000),
        "eval_ms": 0,
        "total_ms": int((t_total - t0) * 1000),
        "succeeded": sum(results),
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--n", type=int, default=10)
    ap.add_argument("--backends", nargs="+", default=["sandstorm", "docker"],
                    choices=["sandstorm", "docker", "cubesandbox", "boxlite", "opensandbox"])
    ap.add_argument("--cube-template", default="",
                    help="CubeSandbox template ID (from cubemastercli tpl create)")
    ap.add_argument("--opensandbox-image",
                    default="opensandbox/code-interpreter:v1.0.2",
                    help="OpenSandbox image to spawn")
    ap.add_argument("--out", default="/tmp/sandstorm-bench-results.json")
    args = ap.parse_args()

    results = []
    for backend in args.backends:
        print(f"[bench] {backend} (n={args.n})...", flush=True)
        if backend == "sandstorm":
            r = bench_sandstorm(args.n)
        elif backend == "docker":
            r = bench_docker(args.n)
        elif backend == "cubesandbox":
            r = bench_cubesandbox(args.n, args.cube_template)
        elif backend == "boxlite":
            r = bench_boxlite(args.n)
        elif backend == "opensandbox":
            r = bench_opensandbox(args.n, args.opensandbox_image)
        print(f"  {r}", flush=True)
        results.append(r)

    with open(args.out, "w") as f:
        json.dump(results, f, indent=2)
    print(f"\nwrote {args.out}", flush=True)


if __name__ == "__main__":
    main()
