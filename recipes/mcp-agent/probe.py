"""Debug probe for fastmcp encoding — temporary, not for commit."""
import asyncio
import os

from mcp import ClientSession, StdioServerParameters
from mcp.client.stdio import stdio_client


async def main() -> None:
    server = StdioServerParameters(
        command="forkd-mcp",
        env={
            "FORKD_URL": "http://127.0.0.1:8889",
            "FORKD_TOKEN": os.environ["FORKD_TOKEN"],
        },
    )
    async with stdio_client(server) as (r, w):
        async with ClientSession(r, w) as s:
            await s.initialize()
            sp = await s.call_tool(
                "spawn_sandboxes", {"snapshot_tag": "coding-agent-fork-prewarm-v1", "n": 3}
            )
            print("=== spawn raw ===")
            print(f"  blocks: {len(sp.content)}")
            for i, b in enumerate(sp.content):
                t = getattr(b, "text", None)
                print(f"  block[{i}]: cls={type(b).__name__} text={t!r}")
            print(f"  structuredContent: {getattr(sp, 'structuredContent', None)!r}")

            import json

            text0 = sp.content[0].text
            decoded = json.loads(text0)
            if isinstance(decoded, list):
                sb_id = decoded[0]["id"]
            elif isinstance(decoded, dict):
                sb_id = decoded["id"]
            else:
                raise RuntimeError(f"weird spawn shape: {decoded!r}")

            ex = await s.call_tool(
                "exec_command",
                {"sandbox_id": sb_id, "args": ["sh", "-c", "echo hi"], "timeout_secs": 5},
            )
            print("=== exec raw ===")
            print(f"  blocks: {len(ex.content)}")
            for i, b in enumerate(ex.content):
                t = getattr(b, "text", None)
                print(f"  block[{i}]: cls={type(b).__name__} text={t!r}")
            print(f"  structuredContent: {getattr(ex, 'structuredContent', None)!r}")

            await s.call_tool("kill_sandbox", {"sandbox_id": sb_id})


asyncio.run(main())
