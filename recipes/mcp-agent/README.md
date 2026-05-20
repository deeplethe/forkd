# `mcp-agent`

End-to-end verification that [`forkd-mcp`](../../sdk/mcp/) works. A
host-side Python "agent" drives the forkd-mcp server over stdio
JSON-RPC — exactly the way Claude Desktop / Cursor / Cline drive it —
and exercises the full BRANCH + fan-out flow.

This recipe is not a guest-side rootfs; it's a **host-side reproducible
demo of the MCP integration**. Run it once after installing forkd-mcp
to confirm the protocol works on your box.

## What the demo does

```
list_tools                 → expect 11 (forkd-mcp 0.2.0)
list_snapshots             → pick first tag
spawn_sandboxes            → 1 source sandbox
exec_command               → `echo hello-from-forkd; uname -r`
branch_sandbox(diff=true)  → snapshot with v0.3 diff mode
spawn_sandboxes(branch)    → fan out 3 grandchildren
kill_sandbox x4            → cleanup
```

Prints `pause_ms` and `diff_physical_bytes` from the BRANCH so you
can see the v0.3 diff-snapshot numbers in your own environment.

## Setup

1. **forkd-controller running and reachable.** Either:
   ```bash
   sudo systemctl start forkd-controller
   # OR
   sudo nohup forkd-controller serve \
     --bind 127.0.0.1:8889 \
     --token-file /etc/forkd/token \
     --snapshot-root /var/lib/forkd/snapshots \
     > /var/log/forkd.log 2>&1 &
   ```

2. **At least one snapshot registered.** Build a quick one with
   `forkd snapshot` (see [main README](../../README.md#operating-in-daemon-mode))
   or pull an existing one from the Hub:
   ```bash
   forkd pull deeplethe/langgraph-react
   ```

3. **Install both pieces of the MCP plumbing:**
   ```bash
   pip install forkd-mcp mcp
   ```

4. **Run the demo:**
   ```bash
   FORKD_TOKEN=$(sudo cat /etc/forkd/token) \
     python3 recipes/mcp-agent/demo.py
   ```

Optional first argument: pin a specific snapshot tag instead of
letting the demo auto-pick the first one.

```bash
FORKD_TOKEN=$(sudo cat /etc/forkd/token) \
  python3 recipes/mcp-agent/demo.py langgraph-react
```

## Expected output

```
[mcp-agent] forkd-mcp registers 11 tools:
  - list_snapshots
  - spawn_sandboxes
  - branch_sandbox
  - create_snapshot
  - wait_for_text
  - list_sandboxes
  - get_sandbox
  - kill_sandbox
  - exec_command
  - eval_code
  - ping_sandbox
[mcp-agent] using snapshot 'langgraph-react'
[mcp-agent] spawned sandbox sb-6a0...
[mcp-agent] exec result:
  stdout: 'hello-from-forkd\n6.1.141\n'
  exit_code: 0
[mcp-agent] BRANCH (diff=true): pause_ms=205 diff_physical_bytes=983040
[mcp-agent] fanned out 3 children from branch:
  - sb-6a0...0001
  - sb-6a0...0002
  - sb-6a0...0003
[mcp-agent] cleaned up grandchildren
[mcp-agent] cleaned up source sandbox sb-6a0...
```

`pause_ms` in the BRANCH line is forkd's v0.3 diff-snapshot pause —
typically 200-300 ms regardless of source memory size on commodity
SSD. See
[`bench/pause-window/RESULTS-v0.3.md`](../../bench/pause-window/RESULTS-v0.3.md)
for the full curve.

## Using this with a real MCP client

The Python script reproduces the same calls Claude Desktop / Cursor /
Cline would make. To drive forkd-mcp from a real client instead:

- **Claude Desktop**: see [`sdk/mcp/README.md#register-with-claude-desktop`](../../sdk/mcp/README.md#register-with-claude-desktop)
- **Cursor**: see [`sdk/mcp/README.md#register-with-cursor`](../../sdk/mcp/README.md#register-with-cursor)
- **Cline**: see [`sdk/mcp/README.md#register-with-cline`](../../sdk/mcp/README.md#register-with-cline)
- **Claude Code**: `claude mcp add forkd --env FORKD_URL=... --env FORKD_TOKEN=... -- forkd-mcp`

Once registered, ask the agent in plain English:

> Spawn a Python sandbox, run `pip install requests` inside it, then BRANCH it with diff snapshots enabled and fan out 3 children from the branch. Show me the BRANCH timing.

The agent picks the right tools (`spawn_sandboxes` → `exec_command` → `branch_sandbox` with `diff=true` → `spawn_sandboxes` again) and reports the metrics. Same flow as this script, less typing.

## Troubleshooting

- **`mcp` import fails** → `pip install mcp>=1.0`
- **`forkd-mcp` not found** → `pip install forkd-mcp>=0.2.0`. The
  script spawns `forkd-mcp` as a subprocess; it must be on `PATH`.
- **`no snapshots on the controller`** → register one via
  `forkd snapshot --tag foo --kernel ... --rootfs ...` or
  `forkd pull deeplethe/langgraph-react`.
- **HTTP 401 from forkd-controller** → token mismatch. Make sure
  `FORKD_TOKEN` env on this script matches what
  `forkd-controller --token-file` was given.
- **HTTP 400 from `branch_sandbox` with `diff=true`** → daemon is
  pre-v0.3.0; either upgrade (`pip install forkd>=0.3.0`) or remove
  `"diff": True` from the script.

## What this proves

If this script runs to completion and prints non-zero `pause_ms`
under 1 second, your end-to-end pipeline is:

- forkd-controller speaking REST correctly
- forkd-mcp wrapping it correctly into MCP tools
- An MCP client (this script) driving it with the correct framing
- v0.3 diff snapshots working under the MCP path

This is the minimum reproducible footprint you'd want before pointing
a real LLM-driven agent at forkd via MCP. If the script fails, debug
here before debugging in Claude Desktop.
