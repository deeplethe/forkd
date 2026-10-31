"""MCP server for sandstorm microVM sandboxes.

Wraps a Sandstorm master's HTTP API (docs/API.md) as MCP tools, so any
MCP-aware client (Claude Desktop, Claude Code, etc.) can use sandstorm as
a code-execution backend.

Config via environment:
- SANDSTORM_URL    — master base URL (default: https://127.0.0.1:8080)
- SANDSTORM_TOKEN  — the master's API token
- SANDSTORM_CA     — PEM bundle to verify an https:// master, usually its
                     pki/ca.pem. A master signs its certificate with a CA of
                     its own, so the public trust store cannot verify it.
                     There is deliberately no way to turn verification off:
                     an unverified control plane carries the API token and
                     every command run inside a sandbox.

Run:
    sandstorm-mcp                       # stdio transport, the MCP default

The server is stateless: each tool call hits the master fresh. The master
owns sandbox state.
"""

from __future__ import annotations

import os
import ssl
from typing import Any

import httpx
from mcp.server.fastmcp import FastMCP

SANDSTORM_URL = os.environ.get("SANDSTORM_URL", "https://127.0.0.1:8080").rstrip("/")
SANDSTORM_TOKEN = os.environ.get("SANDSTORM_TOKEN", "").strip()
SANDSTORM_CA = os.environ.get("SANDSTORM_CA") or None
HTTP_TIMEOUT = float(os.environ.get("SANDSTORM_HTTP_TIMEOUT", "60"))

mcp = FastMCP("sandstorm")


def _ssl_context() -> ssl.SSLContext | None:
    if not SANDSTORM_URL.startswith("https://"):
        return None
    # As the Python SDK's Controller. A context rather than verify=<path>,
    # which httpx deprecated, and cafile= replaces the default trust store
    # rather than adding to it, which is what we want: the master's CA is
    # the only issuer that should be able to sign for it.
    return ssl.create_default_context(cafile=SANDSTORM_CA)


# Built once: loading a PEM per tool call is wasted work, and wait_for_text
# opens a client per poll. A bad SANDSTORM_CA stops the server here.
SSL_CONTEXT = _ssl_context()


def _headers(sandbox_token: str | None = None) -> dict[str, str]:
    h = {"Content-Type": "application/json"}
    if SANDSTORM_TOKEN:
        h["Authorization"] = f"Bearer {SANDSTORM_TOKEN}"
    if sandbox_token is not None:
        h["x-sandstorm-sandbox-token"] = sandbox_token
    return h


def _client(sandbox_token: str | None = None) -> httpx.Client:
    return httpx.Client(
        base_url=SANDSTORM_URL,
        headers=_headers(sandbox_token),
        timeout=HTTP_TIMEOUT,
        verify=SSL_CONTEXT if SSL_CONTEXT is not None else True,
    )


@mcp.tool()
def list_snapshots() -> list[dict[str, Any]]:
    """List parent snapshots registered with the sandstorm controller.

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
        n: Number of children to spawn, 1..256.
        per_child_netns: Accepted and ignored: every sandbox has its own
            network namespace.
        memory_limit_mib: Cgroup memory.max for each child.
        prewarm: When true, the daemon performs a throwaway snapshot
            immediately after restore to amortize the cold-cache
            penalty. Relocates the cold cost from the first BRANCH on
            this sandbox to creation time — useful when you have a
            BRANCH SLO and fan out N>=3 from the same source. Default
            false. See bench/pause-window/RESULTS-v0.2.md in the
            v0.5.3 tag.
        live_fork: v0.4 to v0.5 only; a v0.6 master refuses it with
            410 Gone. Boot the sandbox with a memfd-backed RAM region
            so later branch_sandbox calls can use mode="live"
            (UFFD_WP). Requires kernel 5.7+ and the vendored
            Firecracker fork described in docs/VENDORED-FIRECRACKER.md
            in the v0.5.3 tag.

    Returns the spawned SandboxInfo objects (one per child) with their
    id, pid, guest_addr, etc. Retain each creation response token for
    exec_command, eval_code, ping_sandbox, and wait_for_text; GET/LIST
    responses do not include it.
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

    This is sandstorm's core primitive. Modal does this as their
    proprietary moat; sandstorm is the open-source equivalent.

    Args:
        sandbox_id: Id of the source sandbox (see list_sandboxes).
        tag: Optional name for the new snapshot. When unset the
            daemon generates `branch-<sandbox-id>-<unix-ts>-<hex-suffix>`.
        mode: v0.4+ canonical mode selector. One of "full", "diff",
            "live". Prefer this over the legacy `diff` boolean.
            "live" requires the source to have been spawned with
            live_fork=True; source pause drops to sub-50 ms while
            memory streams from the running parent (UFFD_WP). Mutually
            exclusive with `diff` (daemon returns 400 if both).
        diff: Legacy. Equivalent to mode="diff"; kept so this server
            can drive v0.3.x daemons that don't understand `mode`.
            See bench/pause-window/RESULTS-v0.3.md in the v0.5.3 tag.
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
def wait_for_text(
    sandbox_id: str,
    path: str,
    marker: str,
    timeout_secs: float = 60,
    poll_interval_ms: int = 200,
    sandbox_token: str | None = None,
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
        sandbox_token: Token from the creation response; required by v0.6.

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
        with _client(sandbox_token) as c:
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
    sandbox_token: str | None = None,
) -> dict[str, Any]:
    """Run a subprocess inside a sandbox.

    Example:
        exec_command("sb-abc-0000", ["python3", "-c", "print(2+2)"],
                     sandbox_token="<token-from-creation>")

    sandbox_token: Token from the creation response; required by v0.6.

    Returns: {stdout, stderr, exit_code}.
    """
    body = {"args": args, "timeout_secs": timeout_secs}
    with _client(sandbox_token) as c:
        r = c.post(f"/v1/sandboxes/{sandbox_id}/exec", json=body)
        r.raise_for_status()
        return r.json()


@mcp.tool()
def eval_code(sandbox_id: str, code: str, sandbox_token: str | None = None) -> dict[str, Any]:
    """Evaluate a Python expression against the sandbox's warmed PID-1.

    The parent VM already imported numpy / torch / etc.; eval returns
    in single-digit ms instead of ~100 ms for a fresh subprocess.

    sandbox_token: Token from the creation response; required by v0.6.

    Returns: {result, error, exit_code}.
    """
    with _client(sandbox_token) as c:
        r = c.post(f"/v1/sandboxes/{sandbox_id}/eval", json={"code": code})
        r.raise_for_status()
        return r.json()


@mcp.tool()
def ping_sandbox(sandbox_id: str, sandbox_token: str | None = None) -> dict[str, Any]:
    """Round-trip to the in-guest agent. Returns its health and runtime info.

    sandbox_token: Token from the creation response; required by v0.6.
    """
    with _client(sandbox_token) as c:
        r = c.post(f"/v1/sandboxes/{sandbox_id}/ping")
        r.raise_for_status()
        return r.json()


def main() -> None:
    """Entrypoint for the `sandstorm-mcp` console script."""
    mcp.run()


if __name__ == "__main__":
    main()
