# `openai-swarm`

Swarm-style multi-agent handoff where every handoff **BRANCHes the
forkd sandbox**, so the receiving agent inherits the sender's VM
state (filesystem writes, loaded packages, scratch files) instead
of starting from a cold image.

This recipe works against both the original
[`openai-swarm`](https://github.com/openai/swarm) (archived 2024
reference impl) and the newer
[`openai-agents`](https://github.com/openai/openai-agents-python)
SDK — the forkd integration code is library-agnostic.

## The pitch

Swarm-style frameworks model handoffs as "agent A returns agent B
from a tool call". The next round of the loop runs B with the same
conversation. Useful — but B's *environment* is unchanged: same
sandbox, same /tmp, same imported modules. In plain Swarm there's no
way for B to "pick up where A left off" in the sandbox.

With forkd, **the handoff is the snapshot point**. BRANCH the
sandbox at handoff time; spawn one child from the branch; rewire B's
tool to use the child. Now B inherits everything A built:

```
        agent_researcher                 agent_summarizer
        (in sandbox S)                   (in branched child S')
        ┌──────────────┐                 ┌──────────────┐
        │ /tmp/notes.. │   ──BRANCH──>   │ /tmp/notes.. │  ← inherited
        │ imported X   │   handoff       │ imported X   │  ← inherited
        │ env vars set │                 │ env vars set │  ← inherited
        └──────────────┘                 └──────────────┘
                       ≈ 200 ms with diff=true (v0.3)
```

This is the move that plain Swarm + Docker can't do — Docker has no
"branch a container" verb. forkd gives you the verb, and this
recipe wires it into the Swarm handoff idiom.

## What's in this recipe

| File | Role |
|---|---|
| `demo.py` | The recipe. Contains `ForkdRunner` (the tool wrapper) and `do_handoff` (the forkd-specific BRANCH-on-handoff helper). Drop these two into your project verbatim. |
| `README.md` | this file |

The deliberately compact pattern:

```python
# At handoff time, in the tool body that "returns agent_B":
def transfer_to_summarizer(notes_path: str):
    # ← the one forkd line that buys you state inheritance
    do_handoff(controller, runner, label="researcher-to-summarizer")
    return agent_summarizer
```

## Setup

1. **forkd-controller running** with a Python-capable snapshot.

2. **Host network for the branched child:**
   ```bash
   sudo bash scripts/host-tap.sh
   sudo bash scripts/netns-setup.sh 1
   ```

3. **Install libraries:**
   ```bash
   pip install forkd>=0.3.2
   # optional, only needed for live-run with a real LLM:
   pip install openai-agents      # the active replacement
   # or
   pip install git+https://github.com/openai/swarm  # the archived original
   ```

4. **Run (dry-run, no LLM key needed):**
   ```bash
   FORKD_TOKEN=$(sudo cat /etc/forkd/token) \
     python3 recipes/openai-swarm/demo.py --dry-run
   ```

## Expected output (dry-run)

```
[openai-swarm] using snapshot 'coding-agent-fork-prewarm-v1'
[openai-swarm] source sandbox: sb-...-0001
[openai-swarm] dry-run mode (--dry-run)

[dry-run] agent_researcher: write notes
  stdout: wrote 78 bytes

[dry-run] handoff: researcher → summarizer (BRANCH inside)
[handoff] branched + spawned child in 270ms + 75ms (diff_physical=...)

[dry-run] agent_summarizer (now on sb-...-0002): read inherited notes
  stdout: inherited tag: researcher-pass-1
sum: 31

[dry-run] sanity-check: a non-branched child has no /tmp/notes.json
  fresh sandbox sees: CLEAN
  ✓ confirmed: BRANCH transferred state, fresh spawn did not

[openai-swarm] killed sb-...-0002
[openai-swarm] killed sb-...-0001
```

The CLEAN check at the end is the proof that BRANCH is actually
transferring state and not just spawning a fresh sibling.

## Wiring this into a real Swarm / Agents loop

The library-specific glue is intentionally not shown in `demo.py` —
both `openai-agents` and `swarm` change their API surface frequently
and pinning would rot. The pattern is:

```python
# openai-agents (active SDK):
from openai_agents import Agent, Runner

def transfer_to_summarizer():
    do_handoff(controller, runner, label="r→s")
    return agent_summarizer  # Agents picks this up

agent_researcher = Agent(
    name="researcher",
    instructions="...",
    tools=[runner.run_python, transfer_to_summarizer],
)

Runner.run(agent_researcher, "your prompt").final_output
```

```python
# openai-swarm (archived original):
from swarm import Agent, Swarm

def transfer_to_summarizer():
    do_handoff(controller, runner, label="r→s")
    return agent_summarizer

agent_researcher = Agent(
    name="researcher",
    instructions="...",
    functions=[runner.run_python, transfer_to_summarizer],
)

Swarm().run(agent=agent_researcher, messages=[...])
```

The `do_handoff` line is identical in both. That's the whole point.

## How it compares

| Approach | Per-handoff cost | State inheritance | Isolation |
|---|---|---|---|
| Swarm + shared process | ~0 ms | yes (same process — also no isolation) | none |
| Swarm + Docker per agent | 2-5 s + cold filesystem | no — each container starts fresh | strong |
| **Swarm + forkd (this recipe)** | **~200 ms (Diff BRANCH)** | **yes — full VM state inherited** | **strong (microVM)** |

The middle column is the recipe's contribution: state inheritance
with real isolation, in v0.3's ~200 ms window.

## Troubleshooting

- **HTTP 500 on `branch_sandbox`** → daemon < v0.3.0. Upgrade.
- **`runner.run_python` returns exit_code != 0 after handoff** → the
  branched child's tap is in a different netns than the source's;
  the in-guest agent might be unreachable. Make sure
  `scripts/netns-setup.sh 1` ran successfully and `forkd-child-1`
  exists in `ip netns list`.
- **`sum: 31` mismatch** → if you changed the numbers in
  `dry_run()`, recompute the expected sum. The assertion is on
  exit_code, not stdout, so a wrong sum still passes silently.

## See also

- [`mcp-agent/`](../mcp-agent/) — MCP protocol path
- [`crewai-fanout/`](../crewai-fanout/) — N CrewAI agents on N
  sandboxes, same parent
- [`autogen-branch/`](../autogen-branch/) — AutoGen
  `CodeExecutor` + post-turn BRANCH fanout
- [`langgraph-react/`](../langgraph-react/) — full rootfs + ReAct
  agent + BRANCH mid-thought

## What this proves

If the dry-run runs to completion AND prints the `CLEAN` line, your
stack is:

- forkd-controller doing v0.3 Diff BRANCH in a few hundred ms
- `forkd` Python SDK exposing `branch_sandbox(diff=True)` (v0.3.2+)
- State transfer working end-to-end: writes in sandbox S are visible
  in the BRANCH child, but NOT in an unrelated fresh spawn

That's the minimum reproducible footprint for "Swarm + forkd" you'd
want before wiring this into a real conversational handoff loop.
