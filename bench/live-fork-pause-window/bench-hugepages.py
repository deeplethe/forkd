#!/usr/bin/env python3
"""Hugepage vs baseline spawn latency + live-BRANCH pause-window benchmark.

Compares two spawn configurations back-to-back:
  - baseline:  live_fork=true, hugepages=false  (4 KiB pages)
  - hugepages: live_fork=true, hugepages=true   (2 MiB pages, MFD_HUGETLB)

For each iteration the script:
  1. Spawns N sandboxes, records wall-clock spawn time.
  2. Picks the first sandbox and runs a live BRANCH off it, records pause_ms.
  3. Kills all sandboxes + the branch snapshot.

Iterations are interleaved (baseline, hugepages, baseline, ...) so
cache effects average out rather than stacking on one configuration.

Metrics emitted
---------------
- spawn_ms          wall-clock from POST /v1/sandboxes to last sandbox confirmed
- ms_per_child      spawn_ms / n
- pause_ms          source-VM pause window from the BRANCH response

Output
------
- bench-hugepages.csv  one row per iteration
- stdout summary table: p50/p90/max for both metrics, side by side

Usage (must run as root — FC API sockets are root-owned)::

    sudo python3 bench-hugepages.py \\
        --source-tag python-numpy \\
        --n 100 \\
        --iterations 10

On memory-constrained hosts (< 4 GiB RAM or < 50 free hugepages) reduce
--n to avoid OOM. The script warns when HugePages_Free < n.
"""
import argparse
import json
import os
import shutil
import socket
import statistics
import subprocess
import sys
import time
import urllib.error
import urllib.request

DEFAULT_BIN = os.path.expanduser("~/forkd/target/release/forkd-controller")
DEFAULT_FC = "/usr/local/bin/firecracker"
DEFAULT_SNAP_ROOT = os.path.expanduser("~/.local/share/forkd/snapshots")

WORK = "/tmp/forkd-bench-hugepages"
CSV_PATH = os.path.join(WORK, "bench-hugepages.csv")


# ---------------------------------------------------------------------------
# HTTP helpers
# ---------------------------------------------------------------------------

def http(base_url, method, path, body=None, timeout=120):
    data = json.dumps(body).encode() if body is not None else None
    headers = {"Content-Type": "application/json"} if body is not None else {}
    req = urllib.request.Request(
        f"{base_url}{path}", data=data, method=method, headers=headers
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            raw = resp.read().decode("utf-8", errors="replace")
            return resp.status, json.loads(raw) if raw else None
    except urllib.error.HTTPError as e:
        raw = e.read().decode("utf-8", errors="replace")
        try:
            return e.code, json.loads(raw)
        except json.JSONDecodeError:
            return e.code, raw


def wait_for_healthy(base_url, port, deadline_s=20):
    end = time.time() + deadline_s
    while time.time() < end:
        try:
            s = socket.create_connection(("127.0.0.1", port), timeout=1)
            s.close()
            status, _ = http(base_url, "GET", "/healthz", timeout=2)
            if status == 200:
                return
        except (ConnectionRefusedError, socket.timeout, OSError):
            pass
        time.sleep(0.3)
    raise RuntimeError(f"daemon not healthy after {deadline_s}s")


# ---------------------------------------------------------------------------
# Daemon lifecycle
# ---------------------------------------------------------------------------

def setup_workdir(source_tag, source_dir):
    shutil.rmtree(WORK, ignore_errors=True)
    os.makedirs(f"{WORK}/snapshots", exist_ok=True)
    os.makedirs(f"{WORK}/audit", exist_ok=True)

    # Create a real directory for the snapshot so we can rewrite
    # snapshot.json with paths correct for this machine. Symlink the
    # large binary files (memory.bin, vmstate) to avoid copying GiBs.
    target = f"{WORK}/snapshots/{source_tag}"
    os.makedirs(target, exist_ok=True)

    snap_json_src = os.path.join(source_dir, "snapshot.json")
    with open(snap_json_src) as f:
        snap = json.load(f)

    # Rewrite vmstate and memory paths to point at their actual locations
    # on this machine. Handles snapshots packed on a different host where
    # the absolute paths are baked in (e.g. /home/yangdongxu/...).
    for key in ("vmstate", "memory"):
        if key in snap:
            filename = os.path.basename(snap[key])
            actual = os.path.join(source_dir, filename)
            snap[key] = actual
            # Symlink into work dir so the daemon can find them there too.
            link = os.path.join(target, filename)
            if not os.path.lexists(link):
                os.symlink(actual, link)

    with open(os.path.join(target, "snapshot.json"), "w") as f:
        json.dump(snap, f, indent=2)

    state = {
        "snapshots": {
            source_tag: {
                "tag": source_tag,
                "dir": target,
                "created_at_unix": int(time.time()),
                "status": "ready",
            }
        }
    }
    with open(f"{WORK}/state.json", "w") as f:
        json.dump(state, f, indent=2)


def start_daemon(bin_path, bind):
    log = open(f"{WORK}/controller.log", "wb")
    return subprocess.Popen(
        [
            "sudo", bin_path, "serve",
            "--bind", bind,
            "--state", f"{WORK}/state.json",
            "--snapshot-root", f"{WORK}/snapshots",
            "--audit-log", f"{WORK}/audit/audit.log",
        ],
        stdout=log,
        stderr=log,
        stdin=subprocess.DEVNULL,
    )


def kill_leftovers(bind):
    subprocess.run(
        ["sudo", "pkill", "-f", f"forkd-controller serve --bind {bind}"],
        stderr=subprocess.DEVNULL,
    )
    subprocess.run(
        ["sudo", "pkill", "-9", "-f", "/usr/local/bin/firecracker"],
        stderr=subprocess.DEVNULL,
    )
    time.sleep(0.5)


# ---------------------------------------------------------------------------
# Benchmark core
# ---------------------------------------------------------------------------

def hugepages_free():
    """Read HugePages_Free from /proc/meminfo."""
    try:
        for line in open("/proc/meminfo"):
            if line.startswith("HugePages_Free:"):
                return int(line.split()[1])
    except OSError:
        pass
    return 0


def spawn_sandboxes(base_url, tag, n, hugepages):
    """POST /v1/sandboxes; return (sandbox_ids, spawn_ms)."""
    body = {
        "snapshot_tag": tag,
        "n": n,
        "live_fork": True,
        "hugepages": hugepages,
        "per_child_netns": True,
    }
    t0 = time.time()
    status, resp = http(base_url, "POST", "/v1/sandboxes", body)
    spawn_ms = (time.time() - t0) * 1000
    if status != 201:
        raise RuntimeError(f"spawn HTTP {status}: {resp!r}")
    ids = [s["id"] for s in resp]
    return ids, spawn_ms


def branch_sandbox(base_url, sandbox_id, iteration, hugepages_label, mode):
    """BRANCH sandbox_id with the given mode; return pause_ms and branch tag."""
    tag = f"bench-hp{hugepages_label}-{mode}-{iteration:03d}-{int(time.time() * 1000)}"
    body = {"tag": tag, "mode": mode}
    if mode == "live":
        body["wait"] = True
    status, resp = http(
        base_url,
        "POST",
        f"/v1/sandboxes/{sandbox_id}/branch",
        body,
        timeout=60,
    )
    if status not in (201, 202):
        raise RuntimeError(f"branch HTTP {status}: {resp!r}")
    return resp.get("pause_ms"), tag


def kill_sandboxes(base_url, ids):
    for sid in ids:
        http(base_url, "DELETE", f"/v1/sandboxes/{sid}")


def delete_snapshot(base_url, tag):
    status, _ = http(base_url, "DELETE", f"/v1/snapshots/{tag}")
    if status not in (200, 204):
        print(f"  warn: DELETE snapshot {tag} -> HTTP {status}", file=sys.stderr)


def run_iteration(base_url, tag, n, hugepages, iteration, branch_mode):
    """One full iteration: spawn N → branch first → kill all. Returns row dict."""
    label = "true" if hugepages else "false"

    # Spawn N sandboxes.
    ids, spawn_ms = spawn_sandboxes(base_url, tag, n, hugepages)

    # Branch the first sandbox to get pause_ms.
    pause_ms, branch_tag = branch_sandbox(base_url, ids[0], iteration, label, branch_mode)

    # Cleanup.
    kill_sandboxes(base_url, ids)
    delete_snapshot(base_url, branch_tag)

    return {
        "hugepages": label,
        "n": n,
        "iteration": iteration,
        "spawn_ms": round(spawn_ms, 2),
        "ms_per_child": round(spawn_ms / n, 2),
        "pause_ms": pause_ms,
    }


# ---------------------------------------------------------------------------
# Reporting
# ---------------------------------------------------------------------------

COLS = ["hugepages", "n", "iteration", "spawn_ms", "ms_per_child", "pause_ms"]


def write_csv(rows, path):
    with open(path, "w") as f:
        f.write(",".join(COLS) + "\n")
        for r in rows:
            f.write(",".join("" if r[c] is None else str(r[c]) for c in COLS) + "\n")


def pct(vals, p):
    if not vals:
        return float("nan")
    if len(vals) == 1:
        return vals[0]
    return statistics.quantiles(vals, n=100)[p - 1]


def summarize(rows, n, csv_path):
    write_csv(rows, csv_path)

    by_hp = {"false": [], "true": []}
    for r in rows:
        by_hp[r["hugepages"]].append(r)

    print(f"\n=== SUMMARY  n={n} ===")
    header = (
        f"  {'config':<16}  {'iters':>5}  "
        f"{'spawn_ms p50':>13}  {'p99':>7}  {'max':>7}  "
        f"{'ms/child p50':>13}  {'p99':>7}  "
        f"{'pause_ms p50':>13}  {'p99':>7}  {'max':>7}"
    )
    print(header)
    print("  " + "-" * (len(header) - 2))

    for label in ("false", "true"):
        rs = by_hp[label]
        if not rs:
            continue
        spawns = [r["spawn_ms"] for r in rs]
        per_child = [r["ms_per_child"] for r in rs]
        pauses = [r["pause_ms"] for r in rs if r["pause_ms"] is not None]

        print(
            f"  {'hugepages='+label:<16}  {len(rs):>5}  "
            f"{statistics.median(spawns):>13.1f}  {pct(spawns,99):>7.1f}  {max(spawns):>7.1f}  "
            f"{statistics.median(per_child):>13.2f}  {pct(per_child,99):>7.2f}  "
            f"{statistics.median(pauses) if pauses else float('nan'):>13.1f}  "
            f"{pct(pauses,99) if pauses else float('nan'):>7.1f}  "
            f"{max(pauses) if pauses else float('nan'):>7.1f}"
        )

    # Headline speedup ratios.
    base_rows = by_hp["false"]
    hp_rows = by_hp["true"]
    if base_rows and hp_rows:
        base_p50 = statistics.median(r["spawn_ms"] for r in base_rows)
        hp_p50   = statistics.median(r["spawn_ms"] for r in hp_rows)
        base_p99 = pct([r["spawn_ms"] for r in base_rows], 99)
        hp_p99   = pct([r["spawn_ms"] for r in hp_rows], 99)
        if hp_p50 > 0:
            print(f"\n  spawn speedup p50: {base_p50:.0f}ms → {hp_p50:.0f}ms  ({base_p50/hp_p50:.2f}×)")
        if hp_p99 > 0:
            print(f"  spawn speedup p99: {base_p99:.0f}ms → {hp_p99:.0f}ms  ({base_p99/hp_p99:.2f}×)")

    print(f"\n  CSV written to: {csv_path}")


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main():
    parser = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument("--source-tag", default="python-numpy",
                        help="snapshot tag to spawn from (default: python-numpy)")
    parser.add_argument("--snap-root", default=DEFAULT_SNAP_ROOT,
                        help="directory containing snapshot subdirs")
    parser.add_argument("--controller-bin", default=DEFAULT_BIN,
                        help="path to forkd-controller binary")
    parser.add_argument("--port", type=int, default=8892,
                        help="port for the isolated controller instance")
    parser.add_argument("--n", type=int, default=100,
                        help="sandboxes to spawn per iteration (default: 100)")
    parser.add_argument("--iterations", type=int, default=10,
                        help="iterations per configuration (default: 10)")
    parser.add_argument("--out-csv", default=CSV_PATH,
                        help="path for the output CSV")
    parser.add_argument("--branch-mode", default="diff",
                        choices=["full", "diff", "live"],
                        help="BRANCH mode to use for pause_ms measurement (default: diff)")
    args = parser.parse_args()

    bind = f"127.0.0.1:{args.port}"
    base_url = f"http://{bind}"

    source_dir = os.path.join(args.snap_root, args.source_tag)
    if not os.path.isdir(source_dir):
        sys.exit(f"source snapshot not found: {source_dir}\n"
                 f"run: forkd pull deeplethe/{args.source_tag}")

    # Warn if hugepage pool looks too small for the requested N.
    free_hp = hugepages_free()
    hugepage_bytes_needed = args.n * 2  # rough: each sandbox needs ~2 MiB for memfd
    if free_hp > 0 and free_hp * 2 < hugepage_bytes_needed:
        print(
            f"[!] warning: HugePages_Free={free_hp} ({free_hp * 2} MiB) may be "
            f"insufficient for --n {args.n}. Consider reducing --n or increasing "
            f"/proc/sys/vm/nr_hugepages.",
            file=sys.stderr,
        )

    src_mem = os.path.join(source_dir, "memory.bin")
    src_bytes = os.path.getsize(src_mem) if os.path.exists(src_mem) else None

    print(f"[*] source:      {source_dir}")
    if src_bytes:
        print(f"    memory.bin:  {src_bytes} bytes ({src_bytes // (1024 * 1024)} MiB)")
    print(f"[*] n={args.n}  iterations={args.iterations}  branch-mode={args.branch_mode}  controller={bind}")
    print(f"[*] HugePages_Free={free_hp} ({free_hp * 2} MiB available)")

    kill_leftovers(bind)
    setup_workdir(args.source_tag, source_dir)

    print("[*] starting daemon")
    daemon = start_daemon(args.controller_bin, bind)
    rows = []

    try:
        wait_for_healthy(base_url, args.port)
        print("[+] daemon healthy\n")

        # Interleave baseline and hugepages iterations so thermal /
        # cache effects average out across both configurations.
        for i in range(args.iterations):
            for hugepages in (False, True):
                label = "true" if hugepages else "false"
                print(f"  [hugepages={label} iter={i}] running...", flush=True)
                row = run_iteration(base_url, args.source_tag, args.n, hugepages, i, args.branch_mode)
                rows.append(row)
                print(
                    f"  [hugepages={label} iter={i}] done  "
                    f"spawn={row['spawn_ms']:.0f}ms "
                    f"({row['ms_per_child']:.1f}ms/child) "
                    f"pause={row['pause_ms']}ms"
                )

        summarize(rows, args.n, args.out_csv)

    finally:
        print("\n[*] tearing down")
        subprocess.run(["sudo", "kill", str(daemon.pid)], stderr=subprocess.DEVNULL)
        subprocess.run(
            ["sudo", "pkill", "-9", "-f", "/usr/local/bin/firecracker"],
            stderr=subprocess.DEVNULL,
        )
        time.sleep(0.5)


if __name__ == "__main__":
    try:
        main()
    except Exception as e:
        print(f"\n[!] FAIL: {e}", file=sys.stderr)
        import traceback
        traceback.print_exc()
        sys.exit(1)
