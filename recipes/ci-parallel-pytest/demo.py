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


def batch_spawn(n: int, snap_tag: str, token: str) -> tuple[list[str], float]:
    """One POST /v1/sandboxes with n=N. The daemon's `restore_many`
    spawns all N children atomically — this avoids the
    'operation not supported after starting the microVM' race that
    bites if multiple POST /v1/sandboxes calls fire concurrently
    against the same snapshot.

    Returns (sandbox_ids, total_spawn_wall_clock_ms).
    """
    t0 = time.monotonic()
    spawned = http(
        "POST",
        "/v1/sandboxes",
        token,
        # per_child_netns: each child gets its own network namespace
        # (forkd-child-<i>) so workers don't compete for forkd-tap0.
        {"snapshot_tag": snap_tag, "n": n, "per_child_netns": True},
    )
    spawn_ms = (time.monotonic() - t0) * 1000
    return [s["id"] for s in spawned], spawn_ms


def run_pytest_in_sandbox(
    idx: int, sb_id: str, files: list[str], token: str
) -> dict:
    """Drive an already-spawned child: ping until ready → exec pytest
    → delete. Returns per-worker timing.
    """
    # Wait for the guest agent.
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        try:
            http("POST", f"/v1/sandboxes/{sb_id}/ping", token, body={}, timeout=2)
            break
        except Exception:
            time.sleep(0.1)

    cmd = "cd /opt/test_project && python3 -m pytest -v --tb=short " + " ".join(files)
    args = ["sh", "-c", cmd]
    t_exec = time.monotonic()
    try:
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
            "exec_ms": round(exec_ms, 1),
            "exit_code": result.get("exit_code", -1),
            "stdout_tail": (result.get("stdout") or "").strip().split("\n")[-3:],
        }
    except Exception as e:
        return {
            "worker_idx": idx,
            "files": files,
            "exec_ms": round((time.monotonic() - t_exec) * 1000, 1),
            "exit_code": -1,
            "stdout_tail": [f"ERR: {e}"],
        }
    finally:
        try:
            http("DELETE", f"/v1/sandboxes/{sb_id}", token, timeout=15)
        except Exception:
            pass


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
    t_wall0 = time.monotonic()
    sb_ids, batch_spawn_ms = batch_spawn(len(slices), args.snapshot_tag, args.token)
    print(f"  batch spawn ({len(slices)} children): {batch_spawn_ms:.0f} ms")

    with futures.ThreadPoolExecutor(max_workers=len(slices)) as pool:
        results = list(
            pool.map(
                lambda p: run_pytest_in_sandbox(*p),
                [
                    (i, sb_ids[i], slices[i], args.token)
                    for i in range(len(slices))
                ],
            )
        )
    wall_ms = (time.monotonic() - t_wall0) * 1000

    fail = 0
    for r in results:
        status = "PASS" if r["exit_code"] == 0 else f"FAIL({r['exit_code']})"
        files_short = ",".join(f.split("/")[-1] for f in r["files"])
        print(
            f"  [{r['worker_idx']}] {status}  exec={r['exec_ms']:>5.0f}ms  "
            f"files={files_short}"
        )
        if r["exit_code"] != 0:
            fail += 1
            for line in r["stdout_tail"]:
                print(f"        | {line}")

    exec_ms = [r["exec_ms"] for r in results]
    spawn_per_worker = batch_spawn_ms / len(slices)
    print()
    print(
        f"fan-out wall-clock:  {wall_ms:.0f} ms   "
        f"(batch spawn={batch_spawn_ms:.0f} ms = ~{spawn_per_worker:.0f} ms/worker, "
        f"slowest worker exec={max(exec_ms):.0f} ms)"
    )

    if args.sequential_baseline:
        print()
        print("=== sequential baseline: one child runs the whole suite ===")
        t0 = time.monotonic()
        seq_ids, seq_spawn_ms = batch_spawn(1, args.snapshot_tag, args.token)
        seq = run_pytest_in_sandbox(0, seq_ids[0], TEST_FILES, args.token)
        seq_wall_ms = (time.monotonic() - t0) * 1000
        status = "PASS" if seq["exit_code"] == 0 else f"FAIL({seq['exit_code']})"
        print(
            f"  [0] {status}  spawn={seq_spawn_ms:.0f}ms  "
            f"exec={seq['exec_ms']:.0f}ms"
        )
        speedup = seq_wall_ms / wall_ms if wall_ms > 0 else 0
        print(
            f"sequential wall-clock: {seq_wall_ms:.0f} ms   "
            f"(fan-out speedup: {speedup:.2f}×)"
        )

    return fail


if __name__ == "__main__":
    raise SystemExit(main())
