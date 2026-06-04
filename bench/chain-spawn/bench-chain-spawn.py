#!/usr/bin/env python3
"""v0.5 Phase 5 — diff snapshot chain bench (source-delta variant).

Answers three open v0.5 design questions:

  1. Does spawning from a chain head cost meaningfully more than
     spawning from an equivalent flat snapshot?  (the vmstate-drift
     risk's runtime tax)
  2. Does every layer of the chain restore to a correct VM state?
     (the vmstate-drift risk itself — does `from agent.step3 import
     SIGNATURE` actually work in a child spawned from a 3-deep chain?)
  3. How much on-disk space does chaining save vs storing each
     intermediate as a flat snapshot?  (the use-case for chains)

Why source deltas (not pip install)
-----------------------------------

The original plan was to chain pip installs of numpy → pandas →
scikit-learn. Pre-flight on the bench host found `pip install` hangs
inside the guest sandbox. Probes isolated this to Python's
`ssl.create_default_context()` blocking — a guest-image issue
independent of forkd's chain layer. Filed as a separate follow-up.

To keep Phase 5 honest about what it does and doesn't measure, this
bench uses **Python source file deltas** instead of pip installs:
each chain link injects a small Python module under `/opt/agent/`.
The chain is built off the **python-numpy** base (1.5 GiB) so each
diff link's memory.bin contains only the few dirty pages from the
source write — exactly the per-link delta shape the v0.5 design
optimizes for, just with smaller absolute bytes.

This still answers all three v0.5 design questions empirically.
The "pip install chain" demo angle is filed as a follow-up for once
the guest TLS issue is fixed.

Chain shape under test
----------------------

  L0   demo-pyt                 python 3.12-slim base
  L1   chain-bench-l1-step1     +/opt/agent/step1.py  (linspace + mean, stdlib only)
  L2   chain-bench-l2-step2     +/opt/agent/step2.py  (uses step1, adds std)
  L3   chain-bench-l3-step3     +/opt/agent/step3.py  (uses step2, adds 2x2 solve)

Plus a flat-equivalent:

  Flat chain-bench-flat         demo-pyt + all three steps in one diff

Build phase
-----------

For L1 / L2 / L3 / Flat we invoke the v0.5 Phase 2b CLI verb:

    forkd snapshot-diff --from <parent> --tag <child> \
                        --exec "<shell that writes source file>"

Per-link wall-clock from CLI invoke to CLI return.

Spawn phase
-----------

For each head ∈ {L0, L1, L2, L3, Flat}, N iterations of:
    POST /v1/sandboxes  {snapshot_tag: head, n: 1}
then per-layer probe via /exec, then DELETE.

Output
------

  chain-build.csv     layer, command, build_ms, mem_bin_bytes, vmstate_bytes
  chain-spawn.csv     head, depth, iter, spawn_http_ms
  correctness.csv     head, iter, probe, exit_code, stdout_head

Usage
-----

  python3 bench-chain-spawn.py \\
      --daemon-url http://127.0.0.1:8889 \\
      --token <token> \\
      --base-tag python-numpy \\
      --iterations 10
"""

from __future__ import annotations

import argparse
import csv
import json
import os
import statistics
import subprocess
import sys
import time
import urllib.error
import urllib.request
from dataclasses import dataclass
from pathlib import Path

# ---------------- HTTP helpers ----------------


def http(base_url: str, token: str, method: str, path: str, body=None, timeout=120):
    data = json.dumps(body).encode() if body is not None else None
    headers = {"Authorization": f"Bearer {token}"}
    if body is not None:
        headers["Content-Type"] = "application/json"
    req = urllib.request.Request(
        f"{base_url}{path}", data=data, method=method, headers=headers
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            raw = resp.read()
            if not raw:
                return None
            return json.loads(raw)
    except urllib.error.HTTPError as e:
        body = e.read().decode("utf-8", "replace")
        raise RuntimeError(f"{method} {path} → HTTP {e.code} {body[:400]}") from e


# ---------------- chain definition ----------------


# Per-layer Python module source. Kept compact and self-describing.
# Each module re-exports a SIGNATURE constant the bench probes assert on
# to confirm the layer actually restored.

STEP1_SRC = """
\"\"\"Layer 1: build a sample numeric series.\"\"\"
import statistics

SIGNATURE = "step1@v0.5-phase5"


def sample(n: int = 1024):
    # linspace [0.0, 1.0] without numpy.
    if n < 2:
        return [0.0]
    step = 1.0 / (n - 1)
    return [i * step for i in range(n)]


def mean(arr) -> float:
    return statistics.fmean(arr)
"""

STEP2_SRC = """
\"\"\"Layer 2: extend step1 with dispersion.\"\"\"
import statistics

from step1 import sample, mean

SIGNATURE = "step2@v0.5-phase5"


def std(arr) -> float:
    return statistics.pstdev(arr)


def summarize(n: int = 1024) -> dict:
    a = sample(n)
    return {"mean": mean(a), "std": std(a), "n": len(a)}
"""

STEP3_SRC = """
\"\"\"Layer 3: small linear solve via stdlib, demonstrates depth-3 chain restore.\"\"\"
import math

from step1 import sample
from step2 import summarize

SIGNATURE = "step3@v0.5-phase5"


def solve_smoke() -> float:
    # 2x2 deterministic system: [[2,1],[1,3]] @ x = [1,2]  →  x = (1/5, 3/5)
    a = [[2.0, 1.0], [1.0, 3.0]]
    b = [1.0, 2.0]
    det = a[0][0] * a[1][1] - a[0][1] * a[1][0]
    x = [
        (b[0] * a[1][1] - b[1] * a[0][1]) / det,
        (a[0][0] * b[1] - a[1][0] * b[0]) / det,
    ]
    return math.hypot(*x)


def run() -> dict:
    s = summarize()
    s["solve_norm"] = solve_smoke()
    s["signature"] = SIGNATURE
    return s
"""


def write_source_exec(path: str, source: str) -> str:
    """Build a shell command that writes `source` to `path`.

    The CLI's `--exec` is whitespace-tokenised before submission, so
    we avoid shell metasyntax like `$(...)` (the tokeniser splits it).
    Instead: invoke `sh -c <ONE-LITERAL-STRING>` ourselves at the
    top level so the daemon's agent runs the literal under sh.

    Returns the FULL shell pipeline as a single string suitable
    for passing to the CLI's --exec; the CLI splits on whitespace
    so we actually want to drive `sh -c` ourselves at the daemon level,
    which the CLI's shell_split honors when it sees the first token
    being `sh` followed by `-c` and the rest as one quoted blob.
    """
    import base64
    import shlex
    b64 = base64.b64encode(source.encode("utf-8")).decode("ascii")
    # Parent dir is always /opt/agent for this bench — hardcoded
    # avoids $(dirname …) shell substitution which the CLI's
    # tokeniser would split apart.
    inner = (
        f"mkdir -p /opt/agent && "
        f"echo {b64} | base64 -d > {path} && "
        f"wc -c {path}"
    )
    # Pass to CLI as: sh -c 'inner-shell-pipeline'
    # CLI's shell_split will honor single-quoted blob as one arg.
    return f"sh -c {shlex.quote(inner)}"


@dataclass
class Link:
    tag: str
    parent: str
    exec_cmd: str
    probes: list[str]
    depth: int


def build_chain_spec(base: str) -> list[Link]:
    return [
        Link(
            tag="chain-bench-l1-step1",
            parent=base,
            exec_cmd=write_source_exec("/opt/agent/step1.py", STEP1_SRC),
            probes=[
                "import sys; sys.path.insert(0, '/opt/agent'); "
                "import step1; print(step1.SIGNATURE)",
            ],
            depth=1,
        ),
        Link(
            tag="chain-bench-l2-step2",
            parent="chain-bench-l1-step1",
            exec_cmd=write_source_exec("/opt/agent/step2.py", STEP2_SRC),
            probes=[
                "import sys; sys.path.insert(0, '/opt/agent'); "
                "import step1; print(step1.SIGNATURE)",
                "import sys; sys.path.insert(0, '/opt/agent'); "
                "import step2; print(step2.SIGNATURE)",
            ],
            depth=2,
        ),
        Link(
            tag="chain-bench-l3-step3",
            parent="chain-bench-l2-step2",
            exec_cmd=write_source_exec("/opt/agent/step3.py", STEP3_SRC),
            probes=[
                "import sys; sys.path.insert(0, '/opt/agent'); "
                "import step1; print(step1.SIGNATURE)",
                "import sys; sys.path.insert(0, '/opt/agent'); "
                "import step2; print(step2.SIGNATURE)",
                "import sys; sys.path.insert(0, '/opt/agent'); "
                "import step3; print(step3.run()['signature'])",
            ],
            depth=3,
        ),
    ]


def flat_link(base: str) -> Link:
    """One diff that writes all three files."""
    import base64
    import shlex
    sources = {
        "/opt/agent/step1.py": STEP1_SRC,
        "/opt/agent/step2.py": STEP2_SRC,
        "/opt/agent/step3.py": STEP3_SRC,
    }
    parts = ["mkdir -p /opt/agent"]
    for p, src in sources.items():
        b64 = base64.b64encode(src.encode("utf-8")).decode("ascii")
        parts.append(f"echo {b64} | base64 -d > {p}")
    parts.append("wc -c /opt/agent/step1.py /opt/agent/step2.py /opt/agent/step3.py")
    inner = " && ".join(parts)
    exec_cmd = f"sh -c {shlex.quote(inner)}"
    return Link(
        tag="chain-bench-flat",
        parent=base,
        exec_cmd=exec_cmd,
        probes=[
            "import sys; sys.path.insert(0, '/opt/agent'); "
            "import step1; print(step1.SIGNATURE)",
            "import sys; sys.path.insert(0, '/opt/agent'); "
            "import step2; print(step2.SIGNATURE)",
            "import sys; sys.path.insert(0, '/opt/agent'); "
            "import step3; print(step3.run()['signature'])",
        ],
        depth=1,
    )


# ---------------- build phase ----------------


def build_link(forkd_bin: str, daemon_url: str, token: str, link: Link) -> int:
    cmd = [
        forkd_bin, "snapshot-diff",
        "--from", link.parent,
        "--tag", link.tag,
        "--exec", link.exec_cmd,
        "--exec-timeout-secs", "120",
        "--daemon-url", daemon_url,
        "--daemon-token", token,
    ]
    print(f"[build] {link.tag} ← {link.parent}")
    t0 = time.monotonic()
    res = subprocess.run(cmd, capture_output=True, text=True)
    elapsed_ms = int((time.monotonic() - t0) * 1000)
    if res.returncode != 0:
        print(f"  FAIL exit={res.returncode}")
        print(f"  stdout: {res.stdout[-2000:]}")
        print(f"  stderr: {res.stderr[-2000:]}")
        raise RuntimeError(f"build {link.tag} failed")
    print(f"  done in {elapsed_ms} ms")
    return elapsed_ms


def snapshot_sizes(snap_root: Path, tag: str) -> tuple[int, int]:
    d = snap_root / tag
    mem = d / "memory.bin"
    vm = d / "vmstate"
    mem_logical = mem.stat().st_size if mem.exists() else 0
    vm_logical = vm.stat().st_size if vm.exists() else 0
    return (mem_logical, vm_logical)


def snapshot_physical_kib(snap_root: Path, tag: str) -> int:
    """Disk allocation in KiB via du -sk (counts reflink/sparse correctly)."""
    d = snap_root / tag
    try:
        res = subprocess.run(
            ["du", "-sk", str(d)], capture_output=True, text=True, timeout=30,
        )
        if res.returncode == 0:
            return int(res.stdout.split()[0])
    except Exception:
        pass
    return -1


# ---------------- spawn loop ----------------


def spawn_and_probe(
    daemon_url: str,
    token: str,
    head_tag: str,
    probes: list[str],
) -> tuple[int, list[tuple[str, int, str]]]:
    t0 = time.monotonic()
    spawned = http(
        daemon_url, token, "POST", "/v1/sandboxes",
        {"snapshot_tag": head_tag, "n": 1},
    )
    spawn_ms = int((time.monotonic() - t0) * 1000)
    sb_id = spawned[0]["id"]

    # Wait for guest agent via /ping
    deadline = time.monotonic() + 60
    while time.monotonic() < deadline:
        try:
            http(daemon_url, token, "POST",
                 f"/v1/sandboxes/{sb_id}/ping", body={}, timeout=2)
            break
        except Exception:
            time.sleep(0.1)

    probe_results = []
    for p in probes:
        try:
            r = http(
                daemon_url, token, "POST",
                f"/v1/sandboxes/{sb_id}/exec",
                {"args": ["python3", "-c", p], "timeout_secs": 30},
                timeout=40,
            )
            probe_results.append(
                (p[-60:], r.get("exit_code", -1), (r.get("stdout") or "").strip()[:60])
            )
        except Exception as e:
            probe_results.append((p[-60:], -1, f"ERR: {e}"[:60]))

    try:
        http(daemon_url, token, "DELETE", f"/v1/sandboxes/{sb_id}", timeout=30)
    except Exception:
        pass

    return spawn_ms, probe_results


# ---------------- main ----------------


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--daemon-url", default=os.environ.get(
        "FORKD_URL", "http://127.0.0.1:8889"))
    ap.add_argument("--token", default=os.environ.get("FORKD_TOKEN", ""))
    ap.add_argument("--forkd-bin", default="/home/yangdongxu/forkd/target/release/forkd")
    ap.add_argument("--snap-root", type=Path,
                    default=Path("/home/yangdongxu/.local/share/forkd/snapshots"))
    ap.add_argument("--base-tag", default="demo-pyt")
    ap.add_argument("--iterations", type=int, default=10)
    ap.add_argument("--skip-build", action="store_true")
    ap.add_argument("--out-dir", type=Path, default=Path("."))
    args = ap.parse_args()

    if not args.token:
        print("ERROR: pass --token or set FORKD_TOKEN", file=sys.stderr)
        return 2

    args.out_dir.mkdir(parents=True, exist_ok=True)

    chain = build_chain_spec(args.base_tag)
    flat = flat_link(args.base_tag)

    # ---- BUILD ----
    build_csv = args.out_dir / "chain-build.csv"
    build_rows = []
    if not args.skip_build:
        for link in chain + [flat]:
            try:
                build_ms = build_link(args.forkd_bin, args.daemon_url, args.token, link)
            except RuntimeError as e:
                print(f"BUILD FAIL on {link.tag}: {e}", file=sys.stderr)
                return 3
            mem_b, vm_b = snapshot_sizes(args.snap_root, link.tag)
            phys_kib = snapshot_physical_kib(args.snap_root, link.tag)
            build_rows.append({
                "layer": link.tag,
                "parent": link.parent,
                "depth": link.depth,
                "command_head": link.exec_cmd[:80],
                "build_ms": build_ms,
                "memory_bin_bytes": mem_b,
                "vmstate_bytes": vm_b,
                "snapshot_du_kib": phys_kib,
            })
        with build_csv.open("w", newline="") as f:
            w = csv.DictWriter(f, fieldnames=list(build_rows[0].keys()))
            w.writeheader()
            w.writerows(build_rows)
        print(f"\nbuild rows → {build_csv}")
    else:
        print("--skip-build: reusing existing chain tags")

    base_mem, base_vm = snapshot_sizes(args.snap_root, args.base_tag)
    base_phys = snapshot_physical_kib(args.snap_root, args.base_tag)
    print(f"\nbase {args.base_tag}: memory.bin={base_mem} vmstate={base_vm} du={base_phys}KiB")

    # ---- SPAWN ----
    spawn_csv = args.out_dir / "chain-spawn.csv"
    correct_csv = args.out_dir / "correctness.csv"

    l1_probe = chain[0].probes[0]  # step1 probe — will fail on bare L0 base, expected
    heads = [
        ("L0", args.base_tag, 0, [l1_probe]),
        ("L1", chain[0].tag, 1, chain[0].probes),
        ("L2", chain[1].tag, 2, chain[1].probes),
        ("L3", chain[2].tag, 3, chain[2].probes),
        ("Flat", flat.tag, 1, flat.probes),
    ]

    spawn_rows = []
    correct_rows = []
    for label, tag, depth, probes in heads:
        print(f"\nspawn loop: {label}  tag={tag}  depth={depth}")
        for i in range(args.iterations):
            try:
                spawn_ms, probe_results = spawn_and_probe(
                    args.daemon_url, args.token, tag, probes,
                )
            except Exception as e:
                print(f"  iter {i}: SPAWN FAIL: {e}")
                spawn_rows.append({
                    "head_label": label, "head_tag": tag,
                    "depth": depth, "iter": i,
                    "spawn_http_ms": -1,
                })
                continue
            spawn_rows.append({
                "head_label": label, "head_tag": tag,
                "depth": depth, "iter": i,
                "spawn_http_ms": spawn_ms,
            })
            for probe, exit_code, head_stdout in probe_results:
                correct_rows.append({
                    "head_label": label, "head_tag": tag,
                    "depth": depth, "iter": i,
                    "probe_tail": probe, "exit_code": exit_code,
                    "stdout_head": head_stdout,
                })
            passed = sum(1 for _, e, _ in probe_results if e == 0)
            print(f"  iter {i}: {spawn_ms} ms  probes ok={passed}/{len(probe_results)}")

        head_ms = [r["spawn_http_ms"] for r in spawn_rows
                   if r["head_label"] == label and r["spawn_http_ms"] > 0]
        if head_ms:
            head_ms_sorted = sorted(head_ms)
            p50 = statistics.median(head_ms_sorted)
            p90 = head_ms_sorted[int(0.9 * (len(head_ms_sorted) - 1))]
            print(f"  → p50={p50:.0f}  p90={p90:.0f}  max={max(head_ms_sorted)} ms")

    with spawn_csv.open("w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=list(spawn_rows[0].keys()))
        w.writeheader()
        w.writerows(spawn_rows)
    with correct_csv.open("w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=list(correct_rows[0].keys()))
        w.writeheader()
        w.writerows(correct_rows)
    print(f"\nspawn rows → {spawn_csv}")
    print(f"correctness rows → {correct_csv}")

    # ---- SUMMARY (stdout) ----
    print("\n========== SUMMARY ==========")
    print(f"{'head':<8} {'depth':<6} {'p50_ms':<10} {'p90_ms':<10} {'max_ms':<10} {'n':<4}")
    for label, tag, depth, _ in heads:
        head_ms = [r["spawn_http_ms"] for r in spawn_rows
                   if r["head_label"] == label and r["spawn_http_ms"] > 0]
        if not head_ms:
            print(f"{label:<8} {depth:<6} (no data)")
            continue
        head_ms_sorted = sorted(head_ms)
        p50 = statistics.median(head_ms_sorted)
        p90 = head_ms_sorted[int(0.9 * (len(head_ms_sorted) - 1))]
        print(f"{label:<8} {depth:<6} {p50:<10.0f} {p90:<10.0f} {max(head_ms_sorted):<10} {len(head_ms):<4}")

    print(f"\n{'head':<8} probe-pass-rate")
    for label, tag, depth, _ in heads:
        rs = [r for r in correct_rows if r["head_label"] == label]
        if not rs:
            print(f"{label:<8} (no data)")
            continue
        passed = sum(1 for r in rs if r["exit_code"] == 0)
        print(f"{label:<8} {passed}/{len(rs)}")

    return 0


if __name__ == "__main__":
    sys.exit(main())
