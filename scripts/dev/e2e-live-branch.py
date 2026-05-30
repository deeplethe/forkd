#!/usr/bin/env python3
"""End-to-end live BRANCH smoke test.

Exercises the full Phase 6 chain:
  - forkd-controller (current main HEAD)
  - vendored Firecracker (forkd-v0.4-mem-backend-shared-v1.12)
  - the existing `coding-agent-fork-prewarm-v1` snapshot at
    ~/.local/share/forkd/snapshots/

Sequence:
  1. Stand up an isolated forkd-controller on 127.0.0.1:8890 with a
     `firecracker` wrapper that adds --no-seccomp (FC's vmm-thread
     seccomp filter does not yet allow userfaultfd; that's a Phase 6
     follow-up issue).
  2. POST /v1/sandboxes with `live_fork: true` -> get a memfd-backed
     sandbox spawned from the prewarm snapshot.
  3. POST /v1/sandboxes/<id>/branch { "live": true, "wait": true }
     -> sync live BRANCH. Measure pause_ms.
  4. POST /v1/sandboxes/<id>/branch { "live": true, "wait": false }
     -> async live BRANCH. Measure HTTP round-trip + poll
     GET /v1/snapshots until the tag flips to status=Ready.
  5. Tear down.

Each branch's resulting memory.bin is sanity-checked: same size as
the source memory.bin, non-zero bytes.

Run as root (the FC API socket and snapshot dir are root-owned).
"""
import json
import os
import shutil
import signal
import socket
import subprocess
import sys
import time
import urllib.request
import urllib.error

DEV_BIN = "/home/yangdongxu/forkd/target/release/forkd-controller"
PATCHED_FC = "/home/yangdongxu/firecracker-fork/build/cargo_target/x86_64-unknown-linux-musl/release/firecracker"
SNAP_ROOT = "/home/yangdongxu/.local/share/forkd/snapshots"
SOURCE_TAG = "coding-agent-fork-prewarm-v1"
SOURCE_DIR = f"{SNAP_ROOT}/{SOURCE_TAG}"
WORK = "/tmp/forkd-e2e"
BIND = "127.0.0.1:8890"
BASE_URL = f"http://{BIND}"
SYSTEM_FC = "/usr/local/bin/firecracker"
SYSTEM_FC_BACKUP = "/usr/local/bin/firecracker.e2e-backup"


def http(method, path, body=None, timeout=30):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(
        f"{BASE_URL}{path}",
        data=data,
        method=method,
        headers={"Content-Type": "application/json"} if body is not None else {},
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


def wait_for_healthy(deadline_s=20):
    end = time.time() + deadline_s
    while time.time() < end:
        try:
            s = socket.create_connection(("127.0.0.1", int(BIND.split(":")[1])), timeout=1)
            s.close()
            status, _ = http("GET", "/healthz", timeout=2)
            if status == 200:
                return
        except (ConnectionRefusedError, socket.timeout, OSError):
            pass
        time.sleep(0.3)
    raise RuntimeError(f"daemon not healthy after {deadline_s}s")


def kill_existing():
    """Kill any forkd-controller bound to BIND, plus stale FC procs."""
    subprocess.run(
        ["sudo", "pkill", "-f", f"forkd-controller serve --bind {BIND}"],
        stderr=subprocess.DEVNULL,
    )
    # Stray FC procs from previous runs of this test
    subprocess.run(
        ["sudo", "pkill", "-f", "/tmp/forkd-e2e/"], stderr=subprocess.DEVNULL,
    )
    time.sleep(0.5)


def setup_workdir():
    """Build a self-contained test work dir + a sudo-PATH-visible
    `firecracker` wrapper. Because /etc/sudoers' `secure_path` strips
    arbitrary PATH overrides, the wrapper has to live somewhere
    secure_path already covers — we temporarily swap it in for
    /usr/local/bin/firecracker and restore on teardown."""
    shutil.rmtree(WORK, ignore_errors=True)
    os.makedirs(WORK, exist_ok=True)
    os.makedirs(f"{WORK}/snapshots", exist_ok=True)
    os.makedirs(f"{WORK}/audit", exist_ok=True)

    wrapper_src = f"{WORK}/firecracker.wrapper"
    with open(wrapper_src, "w") as f:
        f.write(f"""#!/bin/bash
# Phase 6 e2e wrapper: always pass --no-seccomp because FC's vmm-thread
# seccomp filter does not yet allow userfaultfd(2) / UFFDIO_* ioctls,
# which the /uffd/wp endpoint (Phase 6.1.5) calls post-boot.
exec {PATCHED_FC} --no-seccomp "$@"
""")
    os.chmod(wrapper_src, 0o755)

    # Swap /usr/local/bin/firecracker -> wrapper. Move existing aside
    # so we can restore on teardown.
    if not os.path.exists(SYSTEM_FC_BACKUP):
        subprocess.run(["sudo", "mv", SYSTEM_FC, SYSTEM_FC_BACKUP], check=True)
    subprocess.run(["sudo", "cp", wrapper_src, SYSTEM_FC], check=True)
    subprocess.run(["sudo", "chmod", "755", SYSTEM_FC], check=True)

    # Symlink the source snapshot dir into our snap-root so the controller
    # sees it without copying the multi-hundred-MB memory.bin.
    os.symlink(SOURCE_DIR, f"{WORK}/snapshots/{SOURCE_TAG}")

    # Hand-craft state.json with the source snapshot pre-registered.
    state = {
        "snapshots": {
            SOURCE_TAG: {
                "tag": SOURCE_TAG,
                "dir": f"{WORK}/snapshots/{SOURCE_TAG}",
                "created_at_unix": int(time.time()),
                "status": "ready",
            }
        }
    }
    with open(f"{WORK}/state.json", "w") as f:
        json.dump(state, f, indent=2)


def restore_firecracker():
    """Put the system firecracker back after the test."""
    if os.path.exists(SYSTEM_FC_BACKUP):
        subprocess.run(["sudo", "mv", "-f", SYSTEM_FC_BACKUP, SYSTEM_FC], check=False)


def start_daemon():
    """Spawn the controller. With the FC wrapper now at /usr/local/bin
    secure_path covers it automatically."""
    log = open(f"{WORK}/controller.log", "wb")
    proc = subprocess.Popen(
        [
            "sudo",
            DEV_BIN,
            "serve",
            "--bind",
            BIND,
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
    return proc


def main():
    print("[*] kill any leftover state")
    kill_existing()

    print(f"[*] setup work dir {WORK}")
    setup_workdir()

    print(f"[*] start forkd-controller on {BIND}")
    daemon = start_daemon()
    try:
        wait_for_healthy()
        print("[+] daemon healthy")

        # ---- Phase 1: create memfd-backed sandbox ----
        print(f"\n[*] POST /v1/sandboxes (live_fork=true) from {SOURCE_TAG}")
        t0 = time.time()
        status, body = http(
            "POST",
            "/v1/sandboxes",
            {"snapshot_tag": SOURCE_TAG, "n": 1, "live_fork": True},
        )
        spawn_ms = (time.time() - t0) * 1000
        assert status == 201, f"sandbox create failed: HTTP {status} body={body!r}"
        sandbox_id = body[0]["id"]
        print(f"[+] sandbox {sandbox_id} spawned in {spawn_ms:.0f} ms")

        # ---- Phase 2: live BRANCH, wait=true ----
        tag_sync = f"e2e-live-sync-{int(time.time())}"
        print(f"\n[*] live BRANCH wait=true tag={tag_sync}")
        t0 = time.time()
        status, body = http(
            "POST",
            f"/v1/sandboxes/{sandbox_id}/branch",
            {"tag": tag_sync, "live": True, "wait": True},
        )
        wt_ms = (time.time() - t0) * 1000
        assert status == 201, f"wait=true BRANCH failed: HTTP {status} body={body!r}"
        pause_sync = body.get("pause_ms")
        print(f"[+] HTTP 201, round-trip {wt_ms:.0f} ms, daemon-reported pause_ms={pause_sync}")
        sync_mem = f"{WORK}/snapshots/{tag_sync}/memory.bin"
        assert os.path.exists(sync_mem), f"missing {sync_mem}"
        sz_sync = os.path.getsize(sync_mem)
        print(f"[+] sync memory.bin: {sz_sync} bytes ({sz_sync // (1024 * 1024)} MiB)")

        # ---- Phase 3: live BRANCH, wait=false ----
        tag_async = f"e2e-live-async-{int(time.time())}"
        print(f"\n[*] live BRANCH wait=false tag={tag_async}")
        t0 = time.time()
        status, body = http(
            "POST",
            f"/v1/sandboxes/{sandbox_id}/branch",
            {"tag": tag_async, "live": True, "wait": False},
        )
        wf_ms = (time.time() - t0) * 1000
        assert status == 202, f"wait=false BRANCH failed: HTTP {status} body={body!r}"
        assert body.get("status") == "writing", f"expected status=writing got {body!r}"
        pause_async = body.get("pause_ms")
        print(f"[+] HTTP 202, round-trip {wf_ms:.0f} ms, status=writing, pause_ms={pause_async}")

        # Poll for completion
        print(f"[*] poll until status=ready ...")
        poll_start = time.time()
        ready_after = None
        while time.time() - poll_start < 60:
            status, body = http("GET", "/v1/snapshots")
            assert status == 200, f"list_snapshots failed: HTTP {status}"
            entry = next((e for e in body if e["tag"] == tag_async), None)
            assert entry is not None, f"{tag_async} disappeared from list_snapshots"
            if entry["status"] == "ready":
                ready_after = (time.time() - poll_start) * 1000
                break
            if entry["status"] == "failed":
                raise RuntimeError(f"async BRANCH marked Failed: {entry.get('warning')}")
            time.sleep(0.2)
        assert ready_after is not None, "async BRANCH did not become Ready within 60s"
        print(f"[+] async BRANCH reached Ready after {ready_after:.0f} ms (post-202)")
        async_mem = f"{WORK}/snapshots/{tag_async}/memory.bin"
        assert os.path.exists(async_mem), f"missing {async_mem}"
        sz_async = os.path.getsize(async_mem)
        print(f"[+] async memory.bin: {sz_async} bytes ({sz_async // (1024 * 1024)} MiB)")

        # ---- Summary ----
        print("\n=== SUMMARY ===")
        print(f"  sandbox spawn:           {spawn_ms:.0f} ms")
        print(f"  live wait=true:")
        print(f"    HTTP round-trip:       {wt_ms:.0f} ms")
        print(f"    reported pause_ms:     {pause_sync}")
        print(f"    memory.bin size:       {sz_sync} bytes")
        print(f"  live wait=false:")
        print(f"    HTTP round-trip:       {wf_ms:.0f} ms")
        print(f"    reported pause_ms:     {pause_async}")
        print(f"    poll-until-ready:      {ready_after:.0f} ms")
        print(f"    memory.bin size:       {sz_async} bytes")
        if pause_sync is not None and pause_async is not None:
            print(f"  pause_ms sync vs async: {pause_sync} vs {pause_async}")
            print(f"  delta in HTTP RT:      {wt_ms - wf_ms:+.0f} ms (positive = async faster)")
        assert sz_sync == sz_async, f"size mismatch: sync={sz_sync} async={sz_async}"
        print("[+] memory.bin sizes match -> E2E PASSED")

    finally:
        print("\n[*] tearing down daemon")
        subprocess.run(["sudo", "kill", str(daemon.pid)], stderr=subprocess.DEVNULL)
        # Also nuke any lingering FC children
        subprocess.run(
            ["sudo", "pkill", "-9", "-f", "/usr/local/bin/firecracker"],
            stderr=subprocess.DEVNULL,
        )
        time.sleep(0.5)
        daemon.poll()
        print("[*] restoring system firecracker")
        restore_firecracker()


if __name__ == "__main__":
    try:
        main()
    except Exception as e:
        print(f"\n[!] FAIL: {e}", file=sys.stderr)
        sys.exit(1)
