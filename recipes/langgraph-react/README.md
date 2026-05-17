# `langgraph-react`

A real LangGraph ReAct agent inside a forkd sandbox. The whole point
of this recipe is **the demo**: branch a running agent mid-thought,
fan out 3 grandchildren with different prompts, watch all three
reach different conclusions from a shared cognitive state.

If you're here for benchmarks, use `recipes/python-numpy/`. If you're
here for the pitch, you're in the right place.

## The pitch

```
                ┌──────────────────────┐
   t=0..t=5s    │  source agent loads  │
                │  context, runs 3     │
                │  ReAct steps, builds │
                │  partial answer      │
                └──────────┬───────────┘
                           │  forkd branch
                ┌──────────┴───────────┐
                ▼          ▼           ▼
       ┌────────────┐┌────────────┐┌─────────────┐
       │ child A    ││ child B    ││ child C     │
       │ hint:      ││ hint:      ││ hint:       │
       │ "be       "││ "lean      ││ "optimize   │
       │  thorough" ││  minimal"  ││  for cost"  │
       └────────────┘└────────────┘└─────────────┘

   all 3 inherit the source's reasoning state (warm KV cache for
   the conversation so far, in-memory tool results, opened files,
   loaded packages) — they diverge only on the next thought.
```

This is the workflow Modal's hidden branching primitive enables but
keeps closed. Now you can do it with an open-source daemon, on your
own hardware.

## What's in this recipe

| File | Role |
|---|---|
| `agent.py` | LangGraph ReAct agent. Runs inside the sandbox. Reads a `/tmp/forkd-hint.txt` every step so children can be perturbed post-branch. |
| `tools.py` | Two synthetic tools the agent can call (`weather`, `search`). Pure functions, no external deps, fully deterministic so the demo is reproducible. |
| `demo.sh` | Host-side orchestrator. Spawns source → starts agent → waits for "ready to branch" message → calls `POST /branch` → spawns 3 children → writes a different hint into each → collects all 4 transcripts. |
| `build.sh` | Builds the parent rootfs (python:3.12-slim + langgraph + langchain-openai + requests). |
| `requirements.txt` | Pinned Python deps so the rootfs build is reproducible. |

## LLM provider

The agent talks to **SiliconFlow** (`https://api.siliconflow.cn/v1`),
an OpenAI-compatible endpoint that hosts Qwen / DeepSeek / etc.
behind one API. Reasons:

1. The author already has a SiliconFlow key configured for
   adjacent projects (HumanIndex) — no new account needed.
2. Cheap enough to run the demo many times during iteration
   (Qwen2.5-7B costs cents per million tokens).
3. OpenAI-compatible API, so the agent code reads as standard
   LangChain — porting to OpenAI / Anthropic / Together is a
   one-line `base_url` change.

Set `SILICONFLOW_API_KEY` in the orchestrator's environment; the
demo script propagates it into the sandbox.

## Why the agent reads a hint file each step

For the **branch-and-diverge** trick to be visible, the children
need to make different decisions after the fork. We can't perturb
their internal LLM weights, and we don't want to restart their
agents from scratch (that defeats the "shared cognitive state"
claim). So we plant a side-channel:

- Each agent step reads `/tmp/forkd-hint.txt` and prepends its
  contents to the system prompt of the next LLM call.
- Parent never writes a hint → empty file → no perturbation.
- After branching, the orchestrator writes a different hint into
  each child (via `forkd-controller exec`).
- The children's next thought is steered by their respective hints.

The agent's *prior* state — conversation history, tool results,
partial reasoning — is shared. Only the *next* prompt differs.
This is the cheapest faithful model of "speculative parallel
exploration on a stateful agent".

## Reproducing the demo

```bash
# 1) Build rootfs (slow — ~5 min: pip install langgraph is heavy)
sudo SILICONFLOW_API_KEY=$SILICONFLOW_API_KEY bash recipes/langgraph-react/build.sh

# 2) Snapshot it via daemon
curl -fsS -H "Authorization: Bearer $FORKD_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"tag":"langgraph","kernel":"/path/to/vmlinux","rootfs":"/path/to/recipes/langgraph-react/parent.ext4","rw":true,"tap":"forkd-tap0","boot_wait_secs":20}' \
  $FORKD_URL/v1/snapshots

# 3) Run the orchestrated demo (writes results/<timestamp>/ in cwd)
export SILICONFLOW_API_KEY=...
export FORKD_URL=http://127.0.0.1:8889
export FORKD_TOKEN=$(cat /etc/forkd/token)
bash recipes/langgraph-react/demo.sh
```

You get a directory with:

- `parent-transcript.jsonl` — the source agent's full step history
- `child-{thorough,minimal,cost}-transcript.jsonl` — each child's history after the divergence
- `summary.md` — a side-by-side comparison of the four final answers
- `timeline.json` — daemon `pause_ms`, per-step wall times, total cost

## What this is NOT

- **Not benchmarked.** Cost / speed of LLM calls dominate
  everything; forkd's contribution is the BRANCH primitive, not
  the model latency.
- **Not a multi-agent framework.** The agent is a single ReAct
  loop. Multi-agent coordination is a separate problem; this
  recipe just shows the fork primitive is sound.
- **Not deterministic across LLM calls.** Even with temperature=0
  the SiliconFlow load balancer can route to different replicas
  and produce different completions. We make the *prompts*
  reproducible; the *outputs* will vary trial-to-trial.

## Status

- 2026-05-17: recipe stub + design + agent code committed. End-
  to-end run pending dev-box rootfs build.
