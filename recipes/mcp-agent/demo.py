#!/usr/bin/env python3
"""End-to-end demo: a host-side Python "agent" that drives the
forkd-mcp server over stdio, exactly the way Claude Desktop / Cursor /
Cline drive it. Reproducible verification that forkd-mcp 0.2.0's
tools actually work end-to-end.

What this script does:

  1. Spawns the forkd-mcp server as a subprocess (stdio transport).
  2. Speaks the JSON-RPC framing the Model Context Protocol uses.
  3. Lists tools, picks a snapshot, spawns 1 sandbox, execs a
     command in it, BRANCHes the sandbox with diff=true, spawns
     children from the branch, prints the diff metrics, cleans up.

Prerequisites:
  - forkd-controller running locally (e.g. `sudo systemctl start
    forkd-controller`) with FORKD_TOKEN written somewhere readable.
  - At least one snapshot registered (any tag will do; the demo
    just spawns from it). `forkd images` to list.
  - `pip install mcp` (Python MCP client SDK >= 1.0).
  - `pip install forkd-mcp` (the server this script drives).

Usage:
  FORKD_TOKEN=$(cat /etc/forkd/token) \\
      python3 recipes/mcp-agent/demo.py [snapshot_tag]

If `snapshot_tag` is omitted, the demo picks the first snapshot
returned by `list_snapshots`.

The point of this script is not to replace a real MCP client — Claude
Desktop / Cursor / Cline all do this better. The point is to give you
a deterministic reproducible verification that the forkd-mcp protocol
works the way the README claims it does.
"""

from __future__ import annotations

import asyncio
import os
import sys
from typing import Any

# `mcp` is the official Python SDK; `stdio_client` spins up the
# server as a subprocess and gives us a typed client. This is the
# same library Claude Desktop's MCP plumbing is built on.
try:
    from mcp import ClientSession, StdioServerParameters
    from mcp.client.stdio import stdio_client
except ImportError as e:
    print(f"missing 'mcp' library: {e}", file=sys.stderr)
    print("install with: pip install mcp", file=sys.stderr)
    sys.exit(2)


def unwrap_tool_result(result: Any) -> Any:
    """Pull a parsed Python object out of an MCP CallToolResult.

    fastmcp encodes tool return values as TextContent whose `.text` is
    a JSON string. Some fastmcp versions JSON-encode the value once,
    others encode twice (the value gets wrapped in `json.dumps` and
    then the whole content block gets serialized too). We try to
    descend until we hit a non-string.
    """
    import json

    if not result.content:
        return None
    block = result.content[0]
    text = getattr(block, "text", None)
    if text is None:
        return None
    value: Any = text
    for _ in range(3):
        if not isinstance(value, str):
            break
        try:
            value = json.loads(value)
        except json.JSONDecodeError:
            break
    return value


async def run(snapshot_tag: str | None) -> None:
    server = StdioServerParameters(
        command="forkd-mcp",
        env={
            "FORKD_URL": os.environ.get("FORKD_URL", "http://127.0.0.1:8889"),
            "FORKD_TOKEN": os.environ.get("FORKD_TOKEN", ""),
        },
    )

    async with stdio_client(server) as (read, write):
        async with ClientSession(read, write) as session:
            await session.initialize()

            tools = await session.list_tools()
            print(f"[mcp-agent] forkd-mcp registers {len(tools.tools)} tools:")
            for t in tools.tools:
                print(f"  - {t.name}")

            # Pick a snapshot
            snapshots = unwrap_tool_result(
                await session.call_tool("list_snapshots", {})
            )
            if not snapshots:
                raise SystemExit(
                    "no snapshots on the controller; build one with `forkd snapshot`"
                )
            tag = snapshot_tag or snapshots[0]["tag"]
            print(f"[mcp-agent] using snapshot '{tag}'")

            # Spawn 1 sandbox
            spawned = unwrap_tool_result(
                await session.call_tool(
                    "spawn_sandboxes",
                    {"snapshot_tag": tag, "n": 1},
                )
            )
            sb = spawned[0]
            sb_id = sb["id"]
            print(f"[mcp-agent] spawned sandbox {sb_id}")

            try:
                # Run a command
                exec_result = unwrap_tool_result(
                    await session.call_tool(
                        "exec_command",
                        {
                            "sandbox_id": sb_id,
                            "args": ["sh", "-c", "echo hello-from-forkd; uname -r"],
                            "timeout_secs": 5,
                        },
                    )
                )
                print("[mcp-agent] exec result:")
                print(
                    "  stdout:",
                    repr(exec_result.get("stdout", "")[:200]),
                )
                print("  exit_code:", exec_result.get("exit_code"))

                # BRANCH (the forkd-specific move)
                branch = unwrap_tool_result(
                    await session.call_tool(
                        "branch_sandbox",
                        {
                            "sandbox_id": sb_id,
                            "tag": f"mcp-demo-branch-{int(asyncio.get_event_loop().time() * 1000)}",
                            "diff": True,
                        },
                    )
                )
                print(
                    f"[mcp-agent] BRANCH (diff=true): pause_ms={branch.get('pause_ms')} "
                    f"diff_physical_bytes={branch.get('diff_physical_bytes')}"
                )

                # Fan out 3 grandchildren from the branch
                kids = unwrap_tool_result(
                    await session.call_tool(
                        "spawn_sandboxes",
                        {"snapshot_tag": branch["tag"], "n": 3},
                    )
                )
                print(f"[mcp-agent] fanned out {len(kids)} children from branch:")
                for k in kids:
                    print(f"  - {k['id']}")

                # Cleanup grandchildren
                for k in kids:
                    await session.call_tool(
                        "kill_sandbox", {"sandbox_id": k["id"]}
                    )
                print("[mcp-agent] cleaned up grandchildren")

            finally:
                # Cleanup source
                await session.call_tool("kill_sandbox", {"sandbox_id": sb_id})
                print(f"[mcp-agent] cleaned up source sandbox {sb_id}")


def main() -> None:
    snapshot_tag = sys.argv[1] if len(sys.argv) > 1 else None
    asyncio.run(run(snapshot_tag))


if __name__ == "__main__":
    main()
