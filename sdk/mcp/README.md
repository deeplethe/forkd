# sandstorm MCP server

An [MCP](https://modelcontextprotocol.io/) server that exposes sandstorm
microVM sandboxes as tools to any MCP-aware client — Claude Desktop,
Claude Code, Cursor, Cline, etc.

## What it lets the agent do

Once registered, the agent can:

| Tool | What |
|---|---|
| `list_snapshots` | See available parent templates |
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
their proprietary moat; sandstorm is the open-source equivalent. See
[`bench/pause-window/RESULTS-v0.3.md`](https://github.com/deeplethe/sandstorm/blob/v0.5.3/bench/pause-window/RESULTS-v0.3.md)
for the measured numbers.

Each tool maps 1:1 onto a sandstorm-controller REST endpoint
([`docs/API.md`](../../docs/API.md)). The server is stateless; the
controller owns sandbox lifecycle.

## Install

```bash
pip install sandstorm-mcp
# or from source:
pip install -e .
```

Requires a Sandstorm master ([README](../../README.md#quick-start)), reachable
on `https://127.0.0.1:8080` by default.

## Configure

Environment variables:

| Var | Default | Purpose |
|---|---|---|
| `SANDSTORM_URL` | `https://127.0.0.1:8080` | Master base URL |
| `SANDSTORM_TOKEN` | *unset* | The master's API token, in `pki/api-token` under its state directory |
| `SANDSTORM_CA` | *unset* | PEM bundle to verify an `https://` master, usually its `pki/ca.pem` |
| `SANDSTORM_HTTP_TIMEOUT` | `60` | Per-request timeout (seconds) |

A master signs its certificate with a CA of its own, so the public trust store
cannot verify it, and until `SANDSTORM_CA` is set every tool fails with
`CERTIFICATE_VERIFY_FAILED`. Get the CA from the master's `pki/ca.pem`, or fetch
it:

```bash
curl -k https://127.0.0.1:8080/ca.pem -o ca.pem
```

`-k` is for this one download. Check `ca.pem` against the fingerprint in a
registration token before you trust it, and give `SANDSTORM_CA` an absolute path,
since an MCP client decides the directory the server starts in. A path that
cannot be read stops the server at startup. There is deliberately no way to turn
verification off: an unverified control plane carries the API token and every
command run inside a sandbox.

`SANDSTORM_CA` replaces the trust store rather than adding to it: the master's CA
is the only issuer that can sign for it.

The certificate names `127.0.0.1`, `localhost` and the master's `--bind` address,
so connect by one of those. A master bound to `0.0.0.0` can be verified only from
its own machine.

## Sandbox credentials (v0.6)

Retain the `token` returned for each sandbox by `spawn_sandboxes`. GET/LIST
responses do not return it. Pass that sandbox's token on every `exec_command`,
`eval_code`, `ping_sandbox`, and `wait_for_text` call, for example:

```python
sandbox = spawn_sandboxes("python", n=1)[0]
exec_command(sandbox["id"], ["echo", "hello"], sandbox_token=sandbox["token"])
eval_code(sandbox["id"], "2+2", sandbox_token=sandbox["token"])
ping_sandbox(sandbox["id"], sandbox_token=sandbox["token"])
wait_for_text(sandbox["id"], "/tmp/output", "READY", sandbox_token=sandbox["token"])
```

`sandbox_token` is sent as `x-sandstorm-sandbox-token`, including every poll.
It is separate from the API Bearer configured with `SANDSTORM_TOKEN`; v0.6
requires both. The tools do not cache sandbox tokens. The parameter defaults
to `None` for older servers; missing or incorrect tokens still cause v0.6
authentication failures.

## Register with Claude Desktop

Add to your `claude_desktop_config.json` (macOS:
`~/Library/Application Support/Claude/claude_desktop_config.json`):

```json
{
  "mcpServers": {
    "sandstorm": {
      "command": "sandstorm-mcp",
      "env": {
        "SANDSTORM_URL": "https://127.0.0.1:8080",
        "SANDSTORM_CA": "/absolute/path/to/ca.pem",
        "SANDSTORM_TOKEN": "<contents-of-/var/lib/sandstorm/pki/api-token>"
      }
    }
  }
}
```

Restart Claude Desktop. The eight tools above will appear in the
"hammer" menu.

## Register with Claude Code

```bash
claude mcp add sandstorm --env SANDSTORM_URL=https://127.0.0.1:8080 \
                     --env SANDSTORM_CA=$PWD/ca.pem \
                     --env SANDSTORM_TOKEN=$(sudo cat /var/lib/sandstorm/pki/api-token) \
                     -- sandstorm-mcp
```

Verify with `claude mcp list`.

## Register with Cursor

Add to `~/.cursor/mcp.json` (or per-workspace `.cursor/mcp.json`):

```json
{
  "mcpServers": {
    "sandstorm": {
      "command": "sandstorm-mcp",
      "env": {
        "SANDSTORM_URL": "https://127.0.0.1:8080",
        "SANDSTORM_CA": "/absolute/path/to/ca.pem",
        "SANDSTORM_TOKEN": "<contents-of-/var/lib/sandstorm/pki/api-token>"
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
    "sandstorm": {
      "command": "sandstorm-mcp",
      "env": {
        "SANDSTORM_URL": "https://127.0.0.1:8080",
        "SANDSTORM_CA": "/absolute/path/to/ca.pem",
        "SANDSTORM_TOKEN": "<contents-of-/var/lib/sandstorm/pki/api-token>"
      },
      "disabled": false,
      "autoApprove": ["list_snapshots", "list_sandboxes", "get_sandbox", "ping_sandbox"]
    }
  }
}
```

The `autoApprove` list is read-only tools that don't need
per-call confirmation. Mutating tools (`spawn_sandboxes`,
`branch_sandbox`, `exec_command`, `kill_sandbox`) always prompt by
default.

## Smoke test

```bash
# In one shell, start a master and a node:
sudo sandstormd serve --master --node

# In another, run the MCP server stand-alone (stdio transport):
curl -k https://127.0.0.1:8080/ca.pem -o ca.pem
SANDSTORM_CA=$PWD/ca.pem SANDSTORM_TOKEN=$(sudo cat /var/lib/sandstorm/pki/api-token) sandstorm-mcp
# The server will block on stdin waiting for an MCP client.
```

To exercise the server without an MCP client, point any MCP debugger
at it (e.g. `npx @modelcontextprotocol/inspector sandstorm-mcp`).

## What this is and isn't

**Is** — a thin wrapper that lets MCP clients drive sandstorm. The agent
plans, the MCP server forwards, the controller actually forks VMs.

**Isn't** — a sandbox itself. sandstorm-controller must be running, and
the host needs KVM + a registered snapshot. See
[`recipes/`](../../recipes/) for ready-to-fork parent images.
