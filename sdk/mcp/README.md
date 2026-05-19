# forkd MCP server

An [MCP](https://modelcontextprotocol.io/) server that exposes forkd
microVM sandboxes as tools to any MCP-aware client — Claude Desktop,
Claude Code, Cursor, Cline, etc.

## What it lets the agent do

Once registered, the agent can:

| Tool | What |
|---|---|
| `list_snapshots` | See available parent templates |
| `create_snapshot` | Build a new snapshot from kernel + rootfs (v0.2.0+) |
| `spawn_sandboxes` | Fork N children from a template. Accepts `prewarm: bool` (v0.2.0+) |
| `branch_sandbox` | **Branch a running sandbox into a new tag (v0.2.0+).** Accepts `diff: bool` for v0.3's 6-15× source-pause reduction on typical agent workloads (143× ceiling). |
| `list_sandboxes` | List live sandboxes |
| `get_sandbox` | Inspect one sandbox by id |
| `exec_command` | Run a shell command in a sandbox |
| `eval_code` | Evaluate Python against the warmed PID-1 |
| `wait_for_text` | Poll a file in the guest for a marker string (v0.2.0+) |
| `ping_sandbox` | Health-check a sandbox |
| `kill_sandbox` | Terminate one sandbox |

The killer one is **`branch_sandbox`**: pause a running agent
sandbox, snapshot, fan out N children that inherit the source's
exact state and diverge under copy-on-write. Modal does this as
their proprietary moat; forkd is the open-source equivalent. See
[`bench/pause-window/RESULTS-v0.3.md`](../../bench/pause-window/RESULTS-v0.3.md)
for the measured numbers.

Each tool maps 1:1 onto a forkd-controller REST endpoint
([`docs/API.md`](../../docs/API.md)). The server is stateless; the
controller owns sandbox lifecycle.

## Install

```bash
pip install forkd-mcp
# or from source:
pip install -e .
```

Requires the forkd-controller daemon running locally
([README](../../README.md#operating-in-daemon-mode)) and reachable
on `http://127.0.0.1:8889` by default.

## Configure

Environment variables:

| Var | Default | Purpose |
|---|---|---|
| `FORKD_URL` | `http://127.0.0.1:8889` | Controller base URL |
| `FORKD_TOKEN` | *unset* | Bearer token, required when daemon is started with `--token-file` |
| `FORKD_HTTP_TIMEOUT` | `60` | Per-request timeout (seconds) |

## Register with Claude Desktop

Add to your `claude_desktop_config.json` (macOS:
`~/Library/Application Support/Claude/claude_desktop_config.json`):

```json
{
  "mcpServers": {
    "forkd": {
      "command": "forkd-mcp",
      "env": {
        "FORKD_URL": "http://127.0.0.1:8889",
        "FORKD_TOKEN": "<contents-of-/etc/forkd/token>"
      }
    }
  }
}
```

Restart Claude Desktop. The eight tools above will appear in the
"hammer" menu.

## Register with Claude Code

```bash
claude mcp add forkd --env FORKD_URL=http://127.0.0.1:8889 \
                     --env FORKD_TOKEN=$(sudo cat /etc/forkd/token) \
                     -- forkd-mcp
```

Verify with `claude mcp list`.

## Register with Cursor

Add to `~/.cursor/mcp.json` (or per-workspace `.cursor/mcp.json`):

```json
{
  "mcpServers": {
    "forkd": {
      "command": "forkd-mcp",
      "env": {
        "FORKD_URL": "http://127.0.0.1:8889",
        "FORKD_TOKEN": "<contents-of-/etc/forkd/token>"
      }
    }
  }
}
```

Restart Cursor (or hit "Refresh" on the MCP settings page).

## Register with Cline

In the Cline extension settings, open "MCP Servers" → "Edit MCP
Settings" and add:

```json
{
  "mcpServers": {
    "forkd": {
      "command": "forkd-mcp",
      "env": {
        "FORKD_URL": "http://127.0.0.1:8889",
        "FORKD_TOKEN": "<contents-of-/etc/forkd/token>"
      },
      "disabled": false,
      "autoApprove": ["list_snapshots", "list_sandboxes", "get_sandbox", "ping_sandbox"]
    }
  }
}
```

The `autoApprove` list is read-only tools that don't need
per-call confirmation. Mutating tools (`spawn_sandboxes`,
`branch_sandbox`, `exec_command`, `kill_sandbox`,
`create_snapshot`) always prompt by default.

## Smoke test

```bash
# In one shell, start the controller:
sudo systemctl start forkd-controller

# In another, run the MCP server stand-alone (stdio transport):
FORKD_TOKEN=$(sudo cat /etc/forkd/token) forkd-mcp
# The server will block on stdin waiting for an MCP client.
```

To exercise the server without an MCP client, point any MCP debugger
at it (e.g. `npx @modelcontextprotocol/inspector forkd-mcp`).

## What this is and isn't

**Is** — a thin wrapper that lets MCP clients drive forkd. The agent
plans, the MCP server forwards, the controller actually forks VMs.

**Isn't** — a sandbox itself. forkd-controller must be running, and
the host needs KVM + a registered snapshot. See
[`recipes/`](../../recipes/) for ready-to-fork parent images.
