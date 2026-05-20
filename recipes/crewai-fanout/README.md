# `crewai-fanout`

A CrewAI crew where every agent runs its tool calls inside its own
dedicated forkd microVM. Same pattern as
[mcp-agent](../mcp-agent/) — host-side integration script, no rootfs
build needed — but driven through the CrewAI Agent/Task/Crew API
instead of the MCP protocol.

## The pitch

CrewAI's Process.parallel runs agents in threads inside one Python
process. That works fine until two agents both `import torch` with
different settings, or one agent's `os.chdir()` confuses another, or
a runaway `while True` in agent A blocks the GIL for the rest. Today
you either accept the contamination risk or you build a Docker-per-
agent harness yourself — and pay 2-5 seconds of cold-start per agent.

forkd gives every agent its own microVM, all forked from one warmed
parent in *milliseconds*. You keep CrewAI's orchestration model and
add real isolation for free.

```
                 ┌──────────────────────────────┐
                 │  parent snapshot (warmed:    │
                 │  python + your imports +     │
                 │  any preloaded data)         │
                 └──────────────┬───────────────┘
                                │ forkd spawn -n N (~ms)
                ┌───────────────┴────────────────┐
                ▼               ▼                ▼
        ┌────────────┐   ┌────────────┐   ┌────────────┐
        │ sandbox 0  │   │ sandbox 1  │   │ sandbox N  │
        │ Agent_0    │   │ Agent_1    │   │ Agent_N    │
        │ tool calls │   │ tool calls │   │ tool calls │
        │ go here    │   │ go here    │   │ go here    │
        └────────────┘   └────────────┘   └────────────┘
```

Each agent has a `ForkdRun` tool — a thin CrewAI `BaseTool` whose
`_run()` exec's Python inside *its* sandbox. The LLM sees N tools
with distinct names and routes its calls accordingly.

## Setup

1. **forkd-controller running.** Either:
   ```bash
   sudo systemctl start forkd-controller
   # OR
   sudo nohup forkd-controller serve \
     --bind 127.0.0.1:8889 \
     --token-file /etc/forkd/token \
     --snapshot-root /var/lib/forkd/snapshots \
     > /var/log/forkd.log 2>&1 &
   ```

2. **At least one Python-capable snapshot.** Use any rootfs that has
   `python3` available — e.g. `coding-agent-fork-prewarm-v1` or one
   you built with `recipes/python-numpy/build.sh`:
   ```bash
   forkd images
   ```

3. **Per-child netns** (needed because n > 1):
   ```bash
   sudo bash scripts/host-tap.sh
   sudo bash scripts/netns-setup.sh 3
   ```

4. **Install the libraries:**
   ```bash
   pip install crewai forkd>=0.3.1
   ```

5. **Set an LLM key** (CrewAI needs one):
   ```bash
   export ANTHROPIC_API_KEY=sk-ant-...
   # OR
   export OPENAI_API_KEY=sk-...
   ```
   Without a key the script runs in dry-run mode — it provisions
   sandboxes, prints the wiring, and exits without calling the LLM.

6. **Run:**
   ```bash
   FORKD_TOKEN=$(sudo cat /etc/forkd/token) \
     python3 recipes/crewai-fanout/demo.py --n=3
   ```

## Expected output (dry-run)

```
[crewai-fanout] using snapshot 'coding-agent-fork-prewarm-v1'
[crewai-fanout] spawned 3 sandboxes in 612.3ms (204.1ms/child)
  - sb-6a0d53e8-0001
  - sb-6a0d53e8-0002
  - sb-6a0d53e8-0003
[crewai-fanout] skipping CrewAI run (no LLM key in env); plan:
  agent[0] → sandbox sb-6a0d53e8-0001 → task: Compute the 25th Fibonacci number...
  agent[1] → sandbox sb-6a0d53e8-0002 → task: Check whether 9973 is prime...
  agent[2] → sandbox sb-6a0d53e8-0003 → task: Sort the list [4, 1, 8, 2, 9, 3]...
[crewai-fanout] cleaned up 3 sandboxes
```

With a key, you'll see CrewAI's agent-by-agent dispatch and the
final aggregated result.

## How it compares

| Approach | Per-agent cold-start | Isolation | Disk overhead |
|---|---|---|---|
| CrewAI default (threads) | ~0 ms | none — shared GIL, sys.path, env | none |
| CrewAI + Docker per agent | 2-5 s | strong | full image × N |
| **CrewAI + forkd (this recipe)** | **~200 ms** | **strong (microVM)** | **diff-snapshot bytes only** |

The interesting middle column is what most CrewAI users want and
can't get with `docker run`: real isolation without the cold-start
tax.

## Adapting to your own crew

The only forkd-specific code is in `demo.py`:

- `provision_sandboxes(...)` — single `controller.spawn_sandboxes()`
  call returns N handles. This is your fanout primitive.
- `make_forkd_tool(...)` — wraps a sandbox handle in a CrewAI
  `BaseTool`. Copy this verbatim into your project; it's the entire
  integration surface.

Everything else — `Agent`, `Task`, `Crew`, the LLM choice — is plain
CrewAI. Replace `SUBTASKS` with your own task list and you have a
fanout crew with per-agent microVM isolation.

## Troubleshooting

- **`forkd` import fails** → `pip install forkd>=0.3.1`
- **`crewai` import fails** → `pip install crewai`
- **No snapshots** → `forkd snapshot --tag foo ...` or
  `forkd pull deeplethe/python-numpy`
- **HTTP 500 on `spawn_sandboxes` with `per_child_netns: true`** →
  you didn't run `scripts/netns-setup.sh N`; the per-child netns
  must exist before spawn.
- **HTTP 401** → `FORKD_TOKEN` env doesn't match the daemon's
  `--token-file`.
- **CrewAI agents don't actually call the tool** → make sure your
  Agent has `allow_delegation=False` and a `goal` that explicitly
  says "use the tool". CrewAI happily lets the LLM compute answers
  in-context if you don't push it. The demo script has language
  that works on Claude/GPT — adjust for other LLMs.

## What this proves

If this script runs to completion and the printed crew result
matches the expected SUBTASKS answers, your stack is:

- forkd-controller speaking REST and forking N microVMs in a single
  call
- `forkd` Python SDK driving it from CrewAI's process
- CrewAI dispatching to N distinct `ForkdRun` tools without
  cross-talk
- Each LLM tool call landing in the correct isolated sandbox

This is the minimum reproducible footprint for "CrewAI + forkd" you'd
want before scaling to a real crew with real workloads.

## See also

- [`mcp-agent/`](../mcp-agent/) — same idea but driven via the MCP
  protocol (Claude Desktop / Cursor / Cline integration path)
- [`langgraph-react/`](../langgraph-react/) — full rootfs recipe + a
  real ReAct agent that BRANCHes mid-thought
