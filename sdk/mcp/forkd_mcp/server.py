"""MCP server for forkd microVM sandboxes.

Wraps the forkd-controller HTTP API (docs/API.md) as MCP tools, so any
MCP-aware client (Claude Desktop, Claude Code, etc.) can use forkd as
a code-execution backend.

Config via environment:
- FORKD_URL    — controller base URL (default: http://127.0.0.1:8889)
- FORKD_TOKEN  — bearer token, when the daemon is started with --token-file

Run:
    forkd-mcp                       # stdio transport, the MCP default

The server is stateless: each tool call hits the controller fresh. The
controller owns sandbox state.
"""

from __future__ import annotations

import os
from typing import Any

import httpx
from mcp.server.fastmcp import FastMCP

FORKD_URL = os.environ.get("FORKD_URL", "http://127.0.0.1:8889").rstrip("/")
FORKD_TOKEN = os.environ.get("FORKD_TOKEN", "").strip()
HTTP_TIMEOUT = float(os.environ.get("FORKD_HTTP_TIMEOUT", "60"))

mcp = FastMCP("forkd")


def _headers() -> dict[str, str]:
    h = {"Content-Type": "application/json"}
    if FORKD_TOKEN:
        h["Authorization"] = f"Bearer {FORKD_TOKEN}"
    return h


def _client() -> httpx.Client:
    return httpx.Client(base_url=FORKD_URL, headers=_headers(), timeout=HTTP_TIMEOUT)


@mcp.tool()
def list_snapshots() -> list[dict[str, Any]]:
    """List parent snapshots registered with the forkd controller.

    Each snapshot is a tagged warmed-VM image you can fork children
    from. Returns SnapshotInfo objects: tag, dir, created_at_unix.
    """
    with _client() as c:
        r = c.get("/v1/snapshots")
        r.raise_for_status()
        return r.json()


@mcp.tool()
def spawn_sandboxes(
    snapshot_tag: str,
    n: int = 1,
    per_child_netns: bool = False,
    memory_limit_mib: int = 256,
    prewarm: bool = False,
    live_fork: bool = False,
) -> list[dict[str, Any]]:
    """Fork N children from a parent snapshot.

    Args:
        snapshot_tag: Name of a registered snapshot (see list_snapshots).
        n: Number of children to spawn, 1..1000.
        per_child_netns: When true, each child is placed in a per-child
            network namespace forkd-child-<i>. The host must have run
            scripts/netns-setup.sh N first.
        memory_limit_mib: Cgroup memory.max for each child.
        prewarm: When true, the daemon performs a throwaway snapshot
            immediately after restore to amortize the cold-cache
            penalty. Relocates the cold cost from the first BRANCH on
            this sandbox to creation time — useful when you have a
            BRANCH SLO and fan out N>=3 from the same source. Default
            false. See bench/pause-window/RESULTS-v0.2.md.
        live_fork: v0.4+. Boot the sandbox with a memfd-backed RAM
            region so later branch_sandbox calls can use mode="live"
            (UFFD_WP). Requires kernel 5.7+ and the vendored
            Firecracker fork — see docs/VENDORED-FIRECRACKER.md.

    Returns the spawned SandboxInfo objects (one per child) with their
    id, pid, guest_addr, etc.
    """
    body = {
        "snapshot_tag": snapshot_tag,
        "n": n,
        "per_child_netns": per_child_netns,
        "memory_limit_mib": memory_limit_mib,
        "prewarm": prewarm,
    }
    if live_fork:
        body["live_fork"] = True
    with _client() as c:
        r = c.post("/v1/sandboxes", json=body)
        r.raise_for_status()
        return r.json()


@mcp.tool()
def branch_sandbox(
    sandbox_id: str,
    tag: str | None = None,
    diff: bool = False,
    measure_diff: bool = False,
    mode: str | None = None,
    wait: bool = True,
) -> dict[str, Any]:
    """Branch a running sandbox into a new snapshot.

    Pauses the source sandbox briefly, snapshots its memory + vCPU
    state, and resumes the source. The resulting snapshot is a new
    tag that any later spawn_sandboxes call can use as its
    snapshot_tag — fan out N children that all inherit the source's
    exact state at branch time.

    This is forkd's core primitive. Modal does this as their
    proprietary moat; forkd is the open-source equivalent.

    Args:
        sandbox_id: Id of the source sandbox (see list_sandboxes).
        tag: Optional name for the new snapshot. When unset the
            daemon generates `branch-<sandbox-id>-<unix-ts>`.
        mode: v0.4+ canonical mode selector. One of "full", "diff",
            "live". Prefer this over the legacy `diff` boolean.
            "live" requires the source to have been spawned with
            live_fork=True; source pause drops to sub-50 ms while
            memory streams from the running parent (UFFD_WP). Mutually
            exclusive with `diff` (daemon returns 400 if both).
        diff: Legacy. Equivalent to mode="diff"; kept so this server
            can drive v0.3.x daemons that don't understand `mode`.
            See bench/pause-window/RESULTS-v0.3.md.
        measure_diff: Measurement-only hook. Take a Diff snapshot
            inside the existing Full pause to report what diff
            would have cost, without changing semantics. Mutually
            exclusive with `diff` (400 if both set).
        wait: v0.4+, only meaningful with mode="live". Default True
            blocks until the background memory copy finishes and the
            returned snapshot is status="ready". Set False to return
            as soon as the source resumes (~10 ms); snapshot reaches
            status="ready" later — poll list_snapshots to detect.

    Returns SnapshotInfo: tag, dir, pause_ms, plus diff_ms /
    diff_physical_bytes / diff_logical_bytes when diff or
    measure_diff was set, and status when mode="live".
    """
    body: dict[str, Any] = {}
    if tag is not None:
        body["tag"] = tag
    # Prefer canonical `mode` when set; fall back to legacy `diff`.
    if mode is not None:
        body["mode"] = mode
    elif diff:
        body["diff"] = True
    if measure_diff:
        body["measure_diff"] = True
    # wait=True is the daemon default; only send when fire-and-forget.
    if not wait:
        body["wait"] = False
    with _client() as c:
        r = c.post(f"/v1/sandboxes/{sandbox_id}/branch", json=body)
        r.raise_for_status()
        return r.json()


@mcp.tool()
def create_snapshot(
    tag: str,
    kernel: str,
    rootfs: str,
    rw: bool = False,
    tap: str | None = None,
    boot_wait_secs: int = 10,
) -> dict[str, Any]:
    """Build a parent snapshot from a kernel + rootfs.

    Boots a fresh VM with the given kernel + rootfs, waits
    `boot_wait_secs` for the guest to settle, snapshots it, and
    registers the snapshot under `tag` so later spawn_sandboxes
    calls can fork children from it.

    Args:
        tag: Name to register the snapshot under (alnum + dash/
            underscore, 1-64 chars).
        kernel: Host path to a vmlinux kernel image.
        rootfs: Host path to a rootfs image (.ext4 for writable,
            .squashfs for read-only).
        rw: When true, mount the rootfs read-write. Auto-enabled
            for .ext4 paths.
        tap: Optional host tap device to attach as guest eth0
            (create with scripts/host-tap.sh).
        boot_wait_secs: Seconds to wait for the guest to settle
            before snapshotting. Bumped to 30+ for snapshots that
            need to warm up large Python packages.

    Returns SnapshotInfo. The snapshot is durable across daemon
    restarts.
    """
    body: dict[str, Any] = {
        "tag": tag,
        "kernel": kernel,
        "rootfs": rootfs,
        "rw": rw,
        "boot_wait_secs": boot_wait_secs,
    }
    if tap is not None:
        body["tap"] = tap
    with _client() as c:
        r = c.post("/v1/snapshots", json=body)
        r.raise_for_status()
        return r.json()


@mcp.tool()
def wait_for_text(
    sandbox_id: str,
    path: str,
    marker: str,
    timeout_secs: float = 60,
    poll_interval_ms: int = 200,
) -> dict[str, Any]:
    """Poll a file inside a sandbox until it contains a marker string.

    Common pattern: agent writes its progress to a stdout log inside
    the guest; the orchestrator (you, the MCP client) polls until a
    marker like "READY_TO_BRANCH" appears, then triggers branch.
    Avoids busy-waiting from outside the daemon's exec round-trip.

    Args:
        sandbox_id: Target sandbox.
        path: Absolute path inside the guest of the file to poll.
        marker: Substring to wait for. Returned as-is when found.
        timeout_secs: Max wall-clock seconds to wait. Default 60.
        poll_interval_ms: How often to re-check. Default 200 ms.

    Returns: {"found": bool, "elapsed_ms": int, "last_excerpt": str}.
    When found is false, last_excerpt contains the last ~256 bytes
    of the file so you can see what's actually being written.
    """
    import time

    deadline = time.monotonic() + timeout_secs
    last_excerpt = ""
    started = time.monotonic()
    while time.monotonic() < deadline:
        # Use exec_command for one-shot read; daemon's exec returns
        # stdout+exit, and a short tail of the target file is cheap.
        with _client() as c:
            r = c.post(
                f"/v1/sandboxes/{sandbox_id}/exec",
                json={
                    "args": ["sh", "-c", f"tail -c 4096 {path} 2>/dev/null || true"],
                    "timeout_secs": 5,
                },
            )
            r.raise_for_status()
            last_excerpt = r.json().get("stdout", "") or ""
            if marker in last_excerpt:
                return {
                    "found": True,
                    "elapsed_ms": int((time.monotonic() - started) * 1000),
                    "last_excerpt": last_excerpt[-256:],
                }
        time.sleep(poll_interval_ms / 1000)
    return {
        "found": False,
        "elapsed_ms": int((time.monotonic() - started) * 1000),
        "last_excerpt": last_excerpt[-256:],
    }


@mcp.tool()
def list_sandboxes() -> list[dict[str, Any]]:
    """List currently-alive child sandboxes."""
    with _client() as c:
        r = c.get("/v1/sandboxes")
        r.raise_for_status()
        return r.json()


@mcp.tool()
def get_sandbox(sandbox_id: str) -> dict[str, Any]:
    """Fetch metadata about one sandbox by id."""
    with _client() as c:
        r = c.get(f"/v1/sandboxes/{sandbox_id}")
        r.raise_for_status()
        return r.json()


@mcp.tool()
def kill_sandbox(sandbox_id: str) -> dict[str, Any]:
    """Terminate one sandbox. Kills Firecracker, removes its cgroup leaf."""
    with _client() as c:
        r = c.delete(f"/v1/sandboxes/{sandbox_id}")
        r.raise_for_status()
        return {"id": sandbox_id, "killed": True}


@mcp.tool()
def exec_command(
    sandbox_id: str,
    args: list[str],
    timeout_secs: int = 30,
) -> dict[str, Any]:
    """Run a subprocess inside a sandbox.

    Example:
        exec_command("sb-abc-0000", ["python3", "-c", "print(2+2)"])

    Returns: {stdout, stderr, exit_code}.
    """
    body = {"args": args, "timeout_secs": timeout_secs}
    with _client() as c:
        r = c.post(f"/v1/sandboxes/{sandbox_id}/exec", json=body)
        r.raise_for_status()
        return r.json()


@mcp.tool()
def eval_code(sandbox_id: str, code: str) -> dict[str, Any]:
    """Evaluate a Python expression against the sandbox's warmed PID-1.

    The parent VM already imported numpy / torch / etc.; eval returns
    in single-digit ms instead of ~100 ms for a fresh subprocess.

    Returns: {result, error, exit_code}.
    """
    with _client() as c:
        r = c.post(f"/v1/sandboxes/{sandbox_id}/eval", json={"code": code})
        r.raise_for_status()
        return r.json()


@mcp.tool()
def ping_sandbox(sandbox_id: str) -> dict[str, Any]:
    """Round-trip to the in-guest agent. Returns its health and runtime info."""
    with _client() as c:
        r = c.post(f"/v1/sandboxes/{sandbox_id}/ping")
        r.raise_for_status()
        return r.json()


def main() -> None:
    """Entrypoint for the `forkd-mcp` console script."""
    mcp.run()


if __name__ == "__main__":
    main()
