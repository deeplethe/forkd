#!/usr/bin/env python3
"""v0.4 live BRANCH pause-window bench.

Drives N iterations of three BRANCH modes off the same live-fork source
sandbox and emits per-iteration CSV plus a p50/p90/max summary. The
point is to get an honest pause_ms number for `mode="live"` against a
known-clean source — Phase 6's E2E used `coding-agent-fork-prewarm-v1`
which has 17 baked guest Oopses contaminating the measurement.

Source selection
----------------

The script symlinks an existing snapshot directory under the script's
work-dir as the source tag. Override `--source-tag` and `--snap-root`
if your snapshots live elsewhere. `python-numpy` is the default
because it's the canonical Hub recipe (`forkd pull
deeplethe/python-numpy`) — anyone with a fresh forkd install can
reproduce against the same bytes.

Setup pattern matches `scripts/dev/e2e-live-branch.py` (Phase 6 E2E):

  1. Stand up an isolated forkd-controller on a free port with a
     `firecracker` wrapper that adds --no-seccomp (the vendored FC's
     vmm seccomp filter blocks userfaultfd; following Phase 6's
     pattern).
  2. POST /v1/sandboxes with `live_fork: true` to spawn a memfd-backed
     source sandbox.
  3. Loop N times for each of {live wait=true, live wait=false, diff,
     full}: POST .../branch, record `pause_ms`, delete the result
     snapshot to keep disk usage bounded.
  4. Emit CSV per iteration + p50/p90/max table to stdout.

Run as root: the FC API socket and snapshot dir are root-owned, and
the system FC swap needs sudo too.

Output
------

- `bench-live-fork.csv` — one row per BRANCH iteration:
    mode, iteration, pause_ms, http_round_trip_ms, memory_bin_bytes,
    poll_until_ready_ms (live wait=false only), source_memory_bytes
- Stdout summary table with p50/p90/max per mode.

Usage:
    sudo python3 bench-live-fork.py \\
        --source-tag python-numpy \\
        --iterations 10 \\
        --modes live-sync,live-async,diff,full
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

# Paths the dev box uses; override via CLI when porting.
DEFAULT_BIN = "/home/yangdongxu/forkd/target/release/forkd-controller"
DEFAULT_FC = (
    "/home/yangdongxu/firecracker-fork/build/cargo_target"
    "/x86_64-unknown-linux-musl/release/firecracker"
)
DEFAULT_SNAP_ROOT = "/home/yangdongxu/.local/share/forkd/snapshots"
SYSTEM_FC = "/usr/local/bin/firecracker"
SYSTEM_FC_BACKUP = "/usr/local/bin/firecracker.bench-live-backup"

WORK = "/tmp/forkd-bench-live"


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


def setup_workdir(source_tag, source_dir, patched_fc):
    shutil.rmtree(WORK, ignore_errors=True)
    os.makedirs(f"{WORK}/snapshots", exist_ok=True)
    os.makedirs(f"{WORK}/audit", exist_ok=True)

    # FC wrapper. Same pattern as the Phase 6 E2E: vendored FC binary
    # + --no-seccomp because the upstream vmm-thread filter still
    # blocks userfaultfd(2).
    wrapper = f"{WORK}/firecracker.wrapper"
    with open(wrapper, "w") as f:
        f.write(
            "#!/bin/bash\n"
            f"exec {patched_fc} --no-seccomp \"$@\"\n"
        )
    os.chmod(wrapper, 0o755)
    if not os.path.exists(SYSTEM_FC_BACKUP):
        subprocess.run(["sudo", "mv", SYSTEM_FC, SYSTEM_FC_BACKUP], check=True)
    subprocess.run(["sudo", "cp", wrapper, SYSTEM_FC], check=True)
    subprocess.run(["sudo", "chmod", "755", SYSTEM_FC], check=True)

    # Symlink the source snapshot dir into our snap-root. Avoids
    # copying the multi-hundred-MB memory.bin.
    target = f"{WORK}/snapshots/{source_tag}"
    if os.path.lexists(target):
        os.unlink(target)
    os.symlink(source_dir, target)

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


def restore_firecracker():
    if os.path.exists(SYSTEM_FC_BACKUP):
        subprocess.run(
            ["sudo", "mv", "-f", SYSTEM_FC_BACKUP, SYSTEM_FC], check=False
        )


def start_daemon(bin_path, bind):
    log = open(f"{WORK}/controller.log", "wb")
    return subprocess.Popen(
        [
            "sudo",
            bin_path,
            "serve",
            "--bind",
            bind,
            "--state",
            f"{WORK}/state.json",
            "--snapshot-root",
            f"{WORK}/snapshots",
            "--audit-log",
            f"{WORK}/audit/audit.log",
        ],
        stdout=log,
        stderr=log,
    )


def kill_leftovers(bind):
    subprocess.run(
        ["sudo", "pkill", "-f", f"forkd-controller serve --bind {bind}"],
        stderr=subprocess.DEVNULL,
    )
    subprocess.run(
        ["sudo", "pkill", "-f", f"{WORK}/"], stderr=subprocess.DEVNULL
    )
    time.sleep(0.5)


def branch_once(base_url, sandbox_id, mode, wait, iteration):
    """Run a single BRANCH; return a per-iteration row dict."""
    tag = f"bench-{mode}-{iteration:03d}-{int(time.time() * 1000)}"
    body = {"tag": tag}
    if mode == "live-sync":
        body["mode"] = "live"
        body["wait"] = True
    elif mode == "live-async":
        body["mode"] = "live"
        body["wait"] = False
    elif mode == "diff":
        body["mode"] = "diff"
    elif mode == "full":
        body["mode"] = "full"
    else:
        raise ValueError(f"unknown mode {mode}")

    t0 = time.time()
    status, resp = http(
        base_url, "POST", f"/v1/sandboxes/{sandbox_id}/branch", body
    )
    rt_ms = (time.time() - t0) * 1000
    if status not in (201, 202):
        raise RuntimeError(f"BRANCH {mode} #{iteration} HTTP {status}: {resp!r}")

    pause_ms = resp.get("pause_ms")
    mem_bytes = None

    ready_ms = None
    if mode == "live-async":
        # Poll until the snapshot flips to status=ready.
        assert status == 202 and resp.get("status") == "writing"
        poll_start = time.time()
        deadline = poll_start + 60
        while time.time() < deadline:
            ls_status, ls = http(base_url, "GET", "/v1/snapshots")
            assert ls_status == 200, f"list_snapshots HTTP {ls_status}"
            entry = next((e for e in ls if e["tag"] == tag), None)
            if entry is None:
                raise RuntimeError(f"{tag} vanished")
            if entry["status"] == "ready":
                ready_ms = (time.time() - poll_start) * 1000
                break
            if entry["status"] == "failed":
                raise RuntimeError(f"{tag} failed: {entry.get('warning')}")
            time.sleep(0.05)
        if ready_ms is None:
            raise RuntimeError(f"{tag} did not reach ready in 60s")

    mem_path = f"{WORK}/snapshots/{tag}/memory.bin"
    if os.path.exists(mem_path):
        mem_bytes = os.path.getsize(mem_path)

    # Delete the snapshot to keep disk usage bounded. The source
    # sandbox isn't affected; only this branch's tag goes away.
    del_status, _ = http(base_url, "DELETE", f"/v1/snapshots/{tag}")
    if del_status not in (200, 204):
        # Non-fatal; bench can keep going, log it.
        print(f"  warn: DELETE {tag} -> HTTP {del_status}", file=sys.stderr)

    return {
        "mode": mode,
        "iteration": iteration,
        "http_round_trip_ms": round(rt_ms, 2),
        "pause_ms": pause_ms,
        "memory_bin_bytes": mem_bytes,
        "poll_until_ready_ms": round(ready_ms, 2) if ready_ms is not None else None,
    }


def summarize(rows, csv_path):
    # Write CSV
    cols = [
        "mode",
        "iteration",
        "http_round_trip_ms",
        "pause_ms",
        "memory_bin_bytes",
        "poll_until_ready_ms",
    ]
    with open(csv_path, "w") as f:
        f.write(",".join(cols) + "\n")
        for r in rows:
            f.write(
                ",".join("" if r[c] is None else str(r[c]) for c in cols) + "\n"
            )

    # Per-mode p50 / p90 / max for pause_ms and round-trip.
    by_mode = {}
    for r in rows:
        by_mode.setdefault(r["mode"], []).append(r)

    print("\n=== SUMMARY ===")
    print(
        f"  {'mode':<14}  {'N':>3}  "
        f"{'pause_ms (p50)':>15}  {'p90':>6}  {'max':>6}  "
        f"{'RT_ms (p50)':>12}  {'p90':>6}  {'max':>6}"
    )
    for mode in ("live-sync", "live-async", "diff", "full"):
        if mode not in by_mode:
            continue
        rs = by_mode[mode]
        pauses = [r["pause_ms"] for r in rs if r["pause_ms"] is not None]
        rts = [r["http_round_trip_ms"] for r in rs]
        if pauses:
            p_p50 = statistics.median(pauses)
            p_p90 = statistics.quantiles(pauses, n=10)[-1] if len(pauses) >= 2 else pauses[0]
            p_max = max(pauses)
        else:
            p_p50 = p_p90 = p_max = float("nan")
        rt_p50 = statistics.median(rts)
        rt_p90 = statistics.quantiles(rts, n=10)[-1] if len(rts) >= 2 else rts[0]
        rt_max = max(rts)
        print(
            f"  {mode:<14}  {len(rs):>3}  "
            f"{p_p50:>15.1f}  {p_p90:>6.1f}  {p_max:>6.1f}  "
            f"{rt_p50:>12.1f}  {rt_p90:>6.1f}  {rt_max:>6.1f}"
        )

    # Headline ratio: live-sync p50 vs diff p50.
    if "live-sync" in by_mode and "diff" in by_mode:
        live_pauses = [
            r["pause_ms"] for r in by_mode["live-sync"] if r["pause_ms"] is not None
        ]
        diff_pauses = [
            r["pause_ms"] for r in by_mode["diff"] if r["pause_ms"] is not None
        ]
        if live_pauses and diff_pauses:
            live_p50 = statistics.median(live_pauses)
            diff_p50 = statistics.median(diff_pauses)
            ratio = diff_p50 / live_p50 if live_p50 > 0 else float("inf")
            print(
                f"\n  diff_p50 / live_p50 = {diff_p50:.0f}/{live_p50:.1f} "
                f"= {ratio:.1f}×"
            )
    print(f"\n  CSV: {csv_path}")


def main():
    parser = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument("--source-tag", default="python-numpy")
    parser.add_argument("--snap-root", default=DEFAULT_SNAP_ROOT)
    parser.add_argument("--controller-bin", default=DEFAULT_BIN)
    parser.add_argument("--patched-fc", default=DEFAULT_FC)
    parser.add_argument(
        "--port", type=int, default=8891, help="port for the isolated controller"
    )
    parser.add_argument(
        "--iterations", type=int, default=10, help="branches per mode"
    )
    parser.add_argument(
        "--modes",
        default="live-sync,live-async,diff,full",
        help="comma-separated subset of {live-sync,live-async,diff,full}",
    )
    parser.add_argument(
        "--out-csv",
        default="/tmp/forkd-bench-live/bench-live-fork.csv",
    )
    args = parser.parse_args()

    bind = f"127.0.0.1:{args.port}"
    base_url = f"http://{bind}"

    source_dir = os.path.join(args.snap_root, args.source_tag)
    if not os.path.isdir(source_dir):
        sys.exit(f"source snapshot not found: {source_dir}")

    # Probe source size — useful for the writeup.
    src_mem = os.path.join(source_dir, "memory.bin")
    src_bytes = os.path.getsize(src_mem) if os.path.exists(src_mem) else None

    modes = args.modes.split(",")
    for m in modes:
        if m not in {"live-sync", "live-async", "diff", "full"}:
            sys.exit(f"unknown mode {m}")

    print(f"[*] source: {source_dir}")
    if src_bytes:
        print(f"    memory.bin: {src_bytes} bytes ({src_bytes // (1024 * 1024)} MiB)")
    print(f"[*] modes: {modes}, iterations per mode: {args.iterations}")
    print(f"[*] controller on {bind}")

    print("[*] kill leftovers")
    kill_leftovers(bind)

    print(f"[*] setup work dir {WORK}")
    setup_workdir(args.source_tag, source_dir, args.patched_fc)

    print("[*] start daemon")
    daemon = start_daemon(args.controller_bin, bind)
    rows = []
    try:
        wait_for_healthy(base_url, args.port)
        print("[+] daemon healthy")

        # Spawn one live-fork source sandbox; all BRANCHes hit it.
        print(f"\n[*] POST /v1/sandboxes live_fork=true tag={args.source_tag}")
        status, body = http(
            base_url,
            "POST",
            "/v1/sandboxes",
            {"snapshot_tag": args.source_tag, "n": 1, "live_fork": True},
        )
        if status != 201:
            raise RuntimeError(f"spawn HTTP {status}: {body!r}")
        sandbox_id = body[0]["id"]
        print(f"[+] sandbox {sandbox_id}")

        # Give the guest a moment to settle (some recipes do post-boot
        # work). Keep it small so the bench's "agent state" isn't
        # dominated by warmup work.
        time.sleep(1.5)

        # Interleave modes so any one-shot effects (cold cache,
        # warm-up, file-system state) average out instead of stacking
        # on the last mode.
        for i in range(args.iterations):
            for m in modes:
                print(f"  [{m} #{i}] ...", end=" ", flush=True)
                row = branch_once(base_url, sandbox_id, m, None, i)
                rows.append(row)
                extra = ""
                if row["poll_until_ready_ms"] is not None:
                    extra = f" ready+{row['poll_until_ready_ms']:.0f}ms"
                print(
                    f"pause={row['pause_ms']}ms "
                    f"rt={row['http_round_trip_ms']:.0f}ms{extra}"
                )

        summarize(rows, args.out_csv)

    finally:
        print("\n[*] tearing down")
        subprocess.run(["sudo", "kill", str(daemon.pid)], stderr=subprocess.DEVNULL)
        subprocess.run(
            ["sudo", "pkill", "-9", "-f", "/usr/local/bin/firecracker"],
            stderr=subprocess.DEVNULL,
        )
        time.sleep(0.5)
        restore_firecracker()


if __name__ == "__main__":
    try:
        main()
    except Exception as e:
        print(f"\n[!] FAIL: {e}", file=sys.stderr)
        sys.exit(1)
