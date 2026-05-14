#!/usr/bin/env python3
"""Fast-path CubeSandbox bench against cube-api on 127.0.0.1:6000.

Pre-warms Python's default ThreadPoolExecutor so its lazy-init isn't
charged to N=1; reports per-call latency for create and kill on top
of total wall-clock. Run on the cube host (cube-api must be local).

Sequence: cold-server N=1 -> ramp 10/50/100 -> warm-server N=1 x2
-> warm-steady N=100. Confirms the N=1 ~ N=10 wall-clock plateau and
isolates the server-side cold-start delta from script artifacts.

Requires: cube-sandbox-one-click running, template `forkd-bench-pynp`
present.
"""

import asyncio
import json
import time
import urllib.request

CUBE_API = "http://127.0.0.1:6000"
TEMPLATE = "forkd-bench-pynp"


def cube_create():
    body = json.dumps({"templateID": TEMPLATE}).encode()
    req = urllib.request.Request(
        f"{CUBE_API}/sandboxes",
        data=body,
        method="POST",
        headers={"Content-Type": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=90) as r:
        return json.load(r)


def cube_kill(sid):
    req = urllib.request.Request(f"{CUBE_API}/sandboxes/{sid}", method="DELETE")
    urllib.request.urlopen(req, timeout=30)


async def one(loop):
    t0 = time.perf_counter()
    r = await loop.run_in_executor(None, cube_create)
    t1 = time.perf_counter()
    sid = r["sandboxID"]
    await loop.run_in_executor(None, cube_kill, sid)
    t2 = time.perf_counter()
    return (t1 - t0) * 1000, (t2 - t1) * 1000


def stats(xs):
    xs = sorted(xs)
    n = len(xs)
    p99 = xs[max(0, int(n * 0.99) - 1)] if n > 1 else xs[0]
    return xs[0], xs[n // 2], p99, xs[-1]


async def run(loop, n, label):
    t0 = time.perf_counter()
    results = await asyncio.gather(*(one(loop) for _ in range(n)))
    t1 = time.perf_counter()
    wall = (t1 - t0) * 1000
    creates = [r[0] for r in results]
    kills = [r[1] for r in results]
    cmin, cp50, cp99, cmax = stats(creates)
    kmin, _, _, kmax = stats(kills)
    print(
        f"[{label}] N={n:3d}  wall={wall:7.1f}ms  succ={len(results)}  "
        f"create min/p50/p99/max = {cmin:6.1f}/{cp50:6.1f}/{cp99:6.1f}/{cmax:6.1f} ms  "
        f"kill min/max = {kmin:5.1f}/{kmax:5.1f} ms",
        flush=True,
    )


async def main():
    loop = asyncio.get_running_loop()

    # Phase 0: warm up the default ThreadPoolExecutor so its lazy
    # initialization isn't charged to the first measured N=1.
    print("--- phase 0: warmup default executor ---", flush=True)
    t0 = time.perf_counter()
    await loop.run_in_executor(None, lambda: None)
    await asyncio.gather(*[loop.run_in_executor(None, lambda: None) for _ in range(8)])
    print(f"executor warmup: {(time.perf_counter() - t0) * 1000:.1f} ms\n", flush=True)

    # Phase 1-4: ramp with cold/warm N=1 markers around it.
    await run(loop, 1, "cold-server")
    await run(loop, 10, "ramp")
    await run(loop, 50, "ramp")
    await run(loop, 100, "ramp")
    await run(loop, 1, "warm-server")
    await run(loop, 1, "warm-server-2")
    await run(loop, 100, "warm-steady")


if __name__ == "__main__":
    asyncio.run(main())
