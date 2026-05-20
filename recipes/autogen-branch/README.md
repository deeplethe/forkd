# `autogen-branch`

An AutoGen `ConversableAgent` whose `CodeExecutor` runs inside a
forkd sandbox — plus a **mid-conversation BRANCH** that fans out N
grandchildren from the same warmed state.

Same shape as [`mcp-agent`](../mcp-agent/) and
[`crewai-fanout`](../crewai-fanout/) — host-side integration script,
no rootfs build needed — but uses AutoGen's official
`autogen_core.code_executor.CodeExecutor` extension point and adds
the BRANCH move that's specific to forkd.

## The pitch

AutoGen ships two production executors today:

- `LocalCommandLineCodeExecutor` — fast, no isolation. One runaway
  `while True` ruins your host.
- `DockerCommandLineCodeExecutor` — strong isolation, 2-5 s cold-
  start per agent, full image disk per container.

Neither lets you do the forkd-shaped move: **branch a conversing
agent mid-turn**. Pause the agent at a decision point, snapshot its
VM state in 200 ms, fan out N alternates that each see a different
next message / system prompt / tool result. Today AutoGen users
either skip this entirely or build it themselves on Docker (with the
matching per-container cold-start tax).

```
            ConversableAgent
            "Compute sum-of-digits of 2**100"
                       │
                       ▼
            ┌──────────────────────┐
            │   forkd sandbox      │  <-- AutoGen's CodeExecutor
            │   python state warm  │      backed by ForkdCommandLine
            │   imports cached     │      CodeExecutor (this recipe)
            │   stdin/stdout open  │
            └──────────┬───────────┘
                       │  forkd branch (≈200 ms)
            ┌──────────┴───────────┐
            ▼          ▼            ▼
       ┌──────────┐┌──────────┐┌──────────┐
       │ child A  ││ child B  ││ child C  │
       │ "try    "││ "try     ││ "try     │
       │ recursion││ stringify││ math.log"│
       └──────────┘└──────────┘└──────────┘
```

## What's in this recipe

| File | Role |
|---|---|
| `demo.py` | Host-side orchestrator. Wraps a sandbox in `ForkdCommandLineCodeExecutor`, drives one ConversableAgent turn, BRANCHes mid-flow, fans out N grandchildren. |
| `README.md` | this file |

## Setup

1. **forkd-controller running** with at least one Python-capable
   snapshot (see `forkd images`).

2. **Per-child netns** for fanout > 1:
   ```bash
   sudo bash scripts/host-tap.sh
   sudo bash scripts/netns-setup.sh 3
   ```

3. **Install libraries:**
   ```bash
   pip install pyautogen forkd>=0.3.1
   ```

4. **Optional LLM key** (`OPENAI_API_KEY` / `AZURE_OPENAI_API_KEY`):
   - With a key: the script runs a real ConversableAgent turn.
   - Without: dry-run mode exercises the executor and BRANCH path
     without calling out to any LLM provider.

5. **Run:**
   ```bash
   FORKD_TOKEN=$(sudo cat /etc/forkd/token) \
     python3 recipes/autogen-branch/demo.py --fanout=3
   ```

## Expected output (dry-run)

```
[autogen-branch] using snapshot 'coding-agent-fork-prewarm-v1'
[autogen-branch] source sandbox: sb-6a0d5598-0001
[autogen-branch] dry-run mode (no LLM key)
[autogen-branch] dry-run exec result (exit=0):
  hello from autogen-branch
  65536
[autogen-branch] BRANCH → tag=autogen-branch-1779242000123 (client-observed 472ms)
[autogen-branch] fanned out 3 grandchildren in 89ms
  sb-...-0002: exit=0 stdout="from-forkd-vm\n(3, 12, 1)"
  sb-...-0003: exit=0 stdout="from-forkd-vm\n(3, 12, 1)"
  sb-...-0004: exit=0 stdout="from-forkd-vm\n(3, 12, 1)"
[autogen-branch] cleaned up 3 grandchildren
[autogen-branch] cleaned up source sandbox sb-6a0d5598-0001
```

With an LLM key you'll see AutoGen's `coder → user → coder` exchange
between the two agents, with the code blocks landing in the forkd
sandbox.

## How it compares

| Executor | Per-call cold-start | Isolation | Mid-state fork |
|---|---|---|---|
| `LocalCommandLineCodeExecutor` | ~0 ms | none | no |
| `DockerCommandLineCodeExecutor` | 2-5 s | strong | no |
| **`ForkdCommandLineCodeExecutor` (this recipe)** | **~200 ms** | **strong (microVM)** | **yes, in ≈200 ms** |

The last column is the forkd-specific value: AutoGen's
`CodeExecutor` interface has no "fork" verb because Docker can't do
it, but forkd can and this recipe exposes it.

## Adapting to your own AutoGen pipeline

The only forkd-specific code is:

- `ForkdCommandLineCodeExecutor` (the inner class in `make_forkd_executor`)
  — implements `autogen_core.code_executor.CodeExecutor`. Copy it
  into your project.
- The BRANCH section at the end of `main()` — single
  `controller.branch_sandbox(sb_id, tag=...)` + one
  `controller.spawn_sandboxes(branch_tag, n=K)` for fanout.

Everything else is plain AutoGen. Replace the dry-run /
`ConversableAgent` driver with your own `GroupChat`, `Swarm`, or
`AssistantAgent` pipeline — the executor and BRANCH primitives are
orthogonal.

## Troubleshooting

- **`autogen_core` import fails** → `pip install pyautogen` (the v0.4+
  series ships `autogen_core`). For older `pyautogen 0.2.x`, the
  CodeExecutor abstraction is in `autogen.coding` instead — the
  `ForkdCommandLineCodeExecutor` shape is the same; adjust the import.
- **HTTP 500 on `branch_sandbox`** → daemon is pre-v0.3.0; upgrade
  (`pip install forkd>=0.3.0`).
- **HTTP 500 on grandchild spawn** → you didn't run
  `scripts/netns-setup.sh N` first; the per-child netns must exist.
- **AutoGen agent says it computed the answer "in its head"** →
  strengthen the system message: "Wrap every computation in a python
  code block; do not compute in-context." LLMs love to skip the tool
  call.

## See also

- [`mcp-agent/`](../mcp-agent/) — MCP-protocol path (Claude Desktop /
  Cursor / Cline)
- [`crewai-fanout/`](../crewai-fanout/) — N CrewAI agents on N
  sandboxes, same parent
- [`langgraph-react/`](../langgraph-react/) — full rootfs + a real
  ReAct agent that BRANCHes mid-thought (includes rootfs build)
