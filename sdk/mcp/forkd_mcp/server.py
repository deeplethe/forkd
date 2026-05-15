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
) -> list[dict[str, Any]]:
    """Fork N children from a parent snapshot.

    Args:
        snapshot_tag: Name of a registered snapshot (see list_snapshots).
        n: Number of children to spawn, 1..1000.
        per_child_netns: When true, each child is placed in a per-child
            network namespace forkd-child-<i>. The host must have run
            scripts/netns-setup.sh N first.
        memory_limit_mib: Cgroup memory.max for each child.

    Returns the spawned SandboxInfo objects (one per child) with their
    id, pid, guest_addr, etc.
    """
    body = {
        "snapshot_tag": snapshot_tag,
        "n": n,
        "per_child_netns": per_child_netns,
        "memory_limit_mib": memory_limit_mib,
    }
    with _client() as c:
        r = c.post("/v1/sandboxes", json=body)
        r.raise_for_status()
        return r.json()


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
