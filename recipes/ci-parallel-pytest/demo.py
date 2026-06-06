#!/usr/bin/env python3
"""Fan-out a pytest suite across N forkd microVMs.

Splits the test_project's tests into N slices (by file), spawns N
children from the `ci-pytest` snapshot, runs one slice per child in
parallel, collects results, and reports total wall-clock vs the
sequential baseline.

For the demo to work the parent must already be built + registered:

    sudo bash recipes/ci-parallel-pytest/build.sh
    sudo forkd snapshot --tag ci-pytest \\
        --kernel /var/lib/forkd/kernels/vmlinux \\
        --rootfs recipes/ci-parallel-pytest/parent.ext4 \\
        --tap forkd-tap0

Then drive it:

    FORKD_TOKEN=$(cat /tmp/bench-pause/token) \\
        python3 recipes/ci-parallel-pytest/demo.py --workers 4

Usage:
    demo.py [--workers N] [--snapshot-tag TAG] [--sequential-baseline]
"""

from __future__ import annotations

import argparse
import concurrent.futures as futures
import json
import os
import time
import urllib.error
import urllib.request

DEFAULT_TAG = "ci-pytest"
DEFAULT_URL = os.environ.get("FORKD_URL", "http://127.0.0.1:8889")


def http(
    method: str, path: str, token: str, body: dict | None = None, timeout: float = 120
) -> dict:
    data = json.dumps(body).encode() if body is not None else None
    headers = {"Authorization": f"Bearer {token}"}
    if body is not None:
        headers["Content-Type"] = "application/json"
    req = urllib.request.Request(
        f"{DEFAULT_URL}{path}", data=data, method=method, headers=headers
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            raw = resp.read()
            return json.loads(raw) if raw else {}
    except urllib.error.HTTPError as e:
        body = e.read().decode("utf-8", "replace")
        raise RuntimeError(f"{method} {path} → HTTP {e.code} {body[:400]}") from e


# The set of test files baked into /opt/test_project/tests/ in the
# `ci-pytest` snapshot. In a real CI setup this would come from
# `pytest --collect-only -q` against the user's project.
TEST_FILES = [
    "tests/test_arithmetic.py",
    "tests/test_numpy_ops.py",
    "tests/test_pandas_etl.py",
    "tests/test_sklearn_models.py",
    "tests/test_text_processing.py",
]


def slice_tests(n_workers: int) -> list[list[str]]:
    """Round-robin assign test files to N worker slices."""
    slices: list[list[str]] = [[] for _ in range(n_workers)]
    for i, f in enumerate(TEST_FILES):
        slices[i % n_workers].append(f)
    return [s for s in slices if s]


def run_one_worker(idx: int, files: list[str], snap_tag: str, token: str) -> dict:
    """Spawn one child sandbox, run its pytest slice, return timing."""
    t0 = time.monotonic()
    spawned = http("POST", "/v1/sandboxes", token, {"snapshot_tag": snap_tag, "n": 1})
    spawn_ms = (time.monotonic() - t0) * 1000
    sb_id = spawned[0]["id"]

    try:
        # Wait for guest agent. /ping with body {} is the lightweight
        # readiness probe the rest of forkd uses.
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            try:
                http("POST", f"/v1/sandboxes/{sb_id}/ping", token, body={}, timeout=2)
                break
            except Exception:
                time.sleep(0.1)

        # Run the pytest slice. Combine all assigned files into one
        # invocation so we pay pytest startup once per worker.
        args = ["python3", "-m", "pytest", "-v", "--tb=short", *files]
        t_exec = time.monotonic()
        result = http(
            "POST",
            f"/v1/sandboxes/{sb_id}/exec",
            token,
            {"args": args, "timeout_secs": 120},
            timeout=130,
        )
        exec_ms = (time.monotonic() - t_exec) * 1000

        return {
            "worker_idx": idx,
            "files": files,
            "spawn_ms": round(spawn_ms, 1),
            "exec_ms": round(exec_ms, 1),
            "exit_code": result.get("exit_code", -1),
            "stdout_tail": (result.get("stdout") or "").strip().split("\n")[-3:],
        }
    finally:
        try:
            http("DELETE", f"/v1/sandboxes/{sb_id}", token, timeout=15)
        except Exception:
            pass


def sequential_baseline(snap_tag: str, token: str) -> dict:
    """One child runs the full suite. The number to beat."""
    return run_one_worker(0, TEST_FILES, snap_tag, token)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--workers", type=int, default=4)
    ap.add_argument("--snapshot-tag", default=DEFAULT_TAG)
    ap.add_argument(
        "--sequential-baseline",
        action="store_true",
        help="Also run the full suite in one child for comparison",
    )
    ap.add_argument(
        "--token",
        default=os.environ.get("FORKD_TOKEN", ""),
        help="Bearer token (or FORKD_TOKEN env)",
    )
    args = ap.parse_args()

    if not args.token:
        print("ERROR: set FORKD_TOKEN env or pass --token")
        return 2

    slices = slice_tests(args.workers)
    print(
        f"Plan: {len(slices)} worker(s) × pytest slice off `{args.snapshot_tag}`."
    )
    for i, s in enumerate(slices):
        print(f"  worker {i}: {len(s)} file(s) — {', '.join(f.split('/')[-1] for f in s)}")
    print()

    print(f"=== fan-out: {len(slices)} workers in parallel ===")
    t0 = time.monotonic()
    with futures.ThreadPoolExecutor(max_workers=len(slices)) as pool:
        results = list(
            pool.map(
                lambda p: run_one_worker(*p),
                [(i, s, args.snapshot_tag, args.token) for i, s in enumerate(slices)],
            )
        )
    wall_ms = (time.monotonic() - t0) * 1000

    fail = 0
    for r in results:
        status = "PASS" if r["exit_code"] == 0 else f"FAIL({r['exit_code']})"
        files_short = ",".join(f.split("/")[-1] for f in r["files"])
        print(
            f"  [{r['worker_idx']}] {status}  spawn={r['spawn_ms']:>5.0f}ms  "
            f"exec={r['exec_ms']:>5.0f}ms  files={files_short}"
        )
        if r["exit_code"] != 0:
            fail += 1
            for line in r["stdout_tail"]:
                print(f"        | {line}")

    spawn_ms = [r["spawn_ms"] for r in results]
    exec_ms = [r["exec_ms"] for r in results]
    print()
    print(
        f"fan-out wall-clock:  {wall_ms:.0f} ms   "
        f"(spawn p50={sorted(spawn_ms)[len(spawn_ms) // 2]:.0f} ms, "
        f"slowest worker exec={max(exec_ms):.0f} ms)"
    )

    if args.sequential_baseline:
        print()
        print("=== sequential baseline: one child runs the whole suite ===")
        seq = run_one_worker(0, TEST_FILES, args.snapshot_tag, args.token)
        status = "PASS" if seq["exit_code"] == 0 else f"FAIL({seq['exit_code']})"
        print(
            f"  [0] {status}  spawn={seq['spawn_ms']:.0f}ms  "
            f"exec={seq['exec_ms']:.0f}ms"
        )
        speedup = seq["exec_ms"] / max(exec_ms) if max(exec_ms) > 0 else 0
        print(
            f"sequential wall-clock: {seq['spawn_ms'] + seq['exec_ms']:.0f} ms   "
            f"(parallel speedup vs slowest worker: {speedup:.2f}×)"
        )

    return fail


if __name__ == "__main__":
    raise SystemExit(main())
