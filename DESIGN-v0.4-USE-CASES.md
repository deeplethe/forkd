# v0.4 use cases — what live-fork unlocks in production

**Status:** scenario inventory, not a roadmap. Concrete answers to
"what does v0.4 enable that v0.3.4 can't, and who needs it?"

The single transition v0.4 makes is **BRANCH pause time crossing the
~50 ms human-perception threshold**, from v0.3.4's ~150 ms (still
visible) to v0.4's ~3 ms (invisible). This converts BRANCH from a
disruptive operation into a transparent control-flow primitive.

```
Perception threshold:     ~50 ms  (below = imperceptible)
v0.3.4 pause window:     ~150 ms  (visible stutter)
v0.4 pause window:         ~3 ms  (transparent)
```

Below that threshold, six classes of system that v0.3.4 cannot
service become viable.

## 1. Speculative agent execution (LLM-step-level branching)

**v0.3.4 limit.** ~6 BRANCHes/second ceiling. Agents that want to
explore N candidates per reasoning step must serialize, which
defeats the latency benefit.

**v0.4 unlock.** 300+ BRANCHes/second. The agent can BRANCH at every
LLM-step decision point, run N candidate continuations in parallel,
score them, keep the best. This is speculative decoding's pattern
applied to whole-sandbox states instead of tokens.

**Concrete product form.**

```
user: "make this codebase use asyncio"
agent (internal):
  ├─ BRANCH-1 (asyncio):  apply patch → run tests → 4 fail
  ├─ BRANCH-2 (trio):     apply patch → run tests → 0 fail
  └─ BRANCH-3 (anyio):    apply patch → run tests → 0 fail + better compat
agent → user: "I used anyio (tests pass + cleaner with 3rd-party libs)"
```

This entire trial-and-judge flow needs every BRANCH under 50 ms or
the agent's response time becomes seconds-per-decision.

**Who buys.** Letta / LangGraph / CrewAI agent platforms, AI coding
assistant teams (Cursor, Continue, Aider).

## 2. Stutter-free interactive agent UX

**v0.3.4 limit.** Parent VM is a streaming service (real-time
conversation, live-coding completion, video pipeline). BRANCHing at
150 ms produces visible artifacts: dropped tokens in streamed
output, half-second pauses mid-conversation, frame drops in video.

**v0.4 unlock.** BRANCH happens between adjacent output tokens or
frames. End-user sees no artifact.

**Concrete product forms.**

- **Live-coding copilot** that BRANCHes 5 candidate completions
  while you type, evaluates each (compile? type-check? matches
  existing style?), shows the winner. Currently impossible because
  the typing pause to BRANCH would be visible.
- **Streaming conversation** where the agent BRANCHes on each user
  utterance to maintain alternative interpretations ("user wants X"
  / "user wants Y") and chooses based on the next utterance.
- **A/B testing in a streaming inference pipeline** — production
  serving stays at version N while a BRANCHed canary serves a
  fraction of requests at version N+1.

**Who buys.** Real-time AI UX teams. The biggest wedge is
"copilots" — the latency budget here is single-digit milliseconds
and v0.3.4 doesn't fit.

## 3. Mass eval rollouts (RL / benchmark suites)

**v0.3.4 limit.** Each rollout starts with a fresh BRANCH from a
warmed parent. 1000 rollouts = 150 seconds of pure BRANCH cost
before any actual task time. Eval suites cap at ~50 rollouts in
practice; full SWE-bench (2000+ tasks) is infeasible from a single
parent.

**v0.4 unlock.** 1000 rollouts ≈ 3 seconds of BRANCH cost. SWE-bench
from a single `python:3.12 + torch + langchain` warmed parent
becomes feasible. RL training loops that need many rollouts per
episode become bounded by the rollout's task time, not by BRANCH
overhead.

**Concrete product form.**

```
1 parent VM (10 GiB, warmed with full Python + ML stack)
       │
       ├─ BRANCH × 2000 (across cluster of hosts)
       │         ↓
       │   each child = independent SWE-bench task (8 GiB CoW share)
       │         ↓
       │   solver runs in parallel
       │
       └─ aggregator: which solver-strategy combo wins this run?
```

The whole eval run hits its real bottleneck (LLM API rate limits
or task wall time), not BRANCH overhead.

**Who buys.** AI research labs running large RL/eval workloads.
Internal teams at OpenAI / Anthropic / DeepMind. Open-source eval
suites (SWE-bench, MLE-bench, Aider's leaderboard).

## 4. Multi-tenant parent sharing

**v0.3.4 limit.** 10 tenants BRANCHing the same warmed parent
serialize: each waits ~150 ms while the parent is paused for the
previous BRANCH. Total: 1.5 s for the last tenant. Sandbox-as-a-
service architectures avoid this by keeping a *pool* of
prewarmed snapshots; pool maintenance is its own engineering tax.

**v0.4 unlock.** All 10 tenants BRANCH concurrently. Each takes
~3 ms WP arm; the parent isn't blocked between them. Pool can
shrink to 1.

**Concrete product form.** Sandbox-as-a-service offering — Modal,
E2B, Daytona, CubeSandbox — runs one warmed parent per
`(image, customer-isolation-class)`. User BRANCHes get parallel
3 ms cost regardless of concurrent tenant count.

**Who buys.** Sandbox-as-a-service platforms. Internal AI infra
teams at large companies running shared agent runtimes for many
employees.

## 5. Always-on stateful parent VM

**v0.3.4 limit.** Parents are designed as "warmed up, snapshotted,
discarded" — they don't survive past the snapshot. The 150 ms
BRANCH cost discourages keeping a parent alive long enough for it
to act as a long-running stateful service.

**v0.4 unlock.** Parents can be persistent stateful services. A
parent VM holding a user's long-term conversation context (8 GiB
of memory: model + agent code + user history + scratch state)
keeps maintaining itself (periodic cache refresh, self-reflection,
embedding updates) while BRANCHing on each user request.

**Concrete product form.**

```
parent VM (lifetime: weeks)
  ├─ holds: user history, agent state, partial computations
  ├─ background: refresh embeddings, prune cache, run scheduled tools
  │
  └─ user request → BRANCH(3 ms) → child handles turn → returns reply
                                   ↑
                                   parent unaffected
```

This is structurally what next-generation ChatGPT/Claude-style
products want: per-session compute that maintains state across
turns rather than reconstructing it from a prompt every time.

**Who buys.** Application teams building long-running personalized
AI assistants. Productivity tools where the agent has continuous
working memory.

## 6. Time-travel debugging for agents

**v0.3.4 limit.** BRANCH-per-checkpoint at the granularity of
"every few LLM steps" because finer granularity is too expensive.
When something goes wrong, you re-run from the nearest checkpoint
with print-debugging.

**v0.4 unlock.** BRANCH-per-LLM-token becomes affordable. The
entire agent execution becomes a fork tree: every token has a
sandbox state captured. When the agent fails, you load the
sandbox state at the exact decision point, modify, and replay.

**Concrete product form.** Developer tool —

```
[failed agent run, opened in forkd-trace UI]

  token 1: "I need to..." (sandbox @ T+0.001s)
  token 2: "...search the docs..."     (sandbox @ T+0.064s)
  token 3: "...for asyncio handling..."(sandbox @ T+0.127s)
        ↑
        [Open sandbox here] — restored sandbox shell, prompt
        modifications, partial state edits, then [Replay from here]
```

This converts agent debugging from "re-run with prints" to "git
checkout to any point in the agent's life".

**Who buys.** Developer-tools teams. Internal agent platforms that
need to debug production agent failures.

## What v0.4 does NOT solve

Honest limits, to avoid overselling:

- **Total throughput per snapshot.** v0.4 moves memory.bin write
  out of the pause window but doesn't speed it up. fsync of 1 GiB
  to ext4 still takes ~5 seconds. Mass BRANCH throughput is bounded
  by storage, not by pause window.
- **Storage cost.** 1000 BRANCHes still consume 1000 × per-child
  divergence on disk (CoW only helps for unmodified pages).
- **Cross-host BRANCH.** Not in v0.4. v0.5 candidate.
- **Cold-start cost.** Parent VM boot + warmup is still seconds-to-
  minutes. v0.4 optimizes BRANCH, not boot.
- **Small parents have low ROI.** A 256 MiB parent's v0.3.4 pause
  is already ~50 ms; v0.4 reducing it to 1 ms is incremental. The
  big wins are at 1 GiB+ parents.

## Priority for launch positioning

If forkd v0.4 ships with a single hero scenario, **#1 (speculative
agent) + #3 (mass eval)** are the most defensible:

- **#1 is the most visual** — "agent tries 3 strategies in parallel,
  picks the winner" is the same demo we have today (`recipes/
  speculative-agent/`) but with v0.4 the **decision becomes
  invisible to the user**. The narrative writes itself.
- **#3 is the most quantitative** — "SWE-bench eval time -90%" is
  the kind of number HN/Twitter rewards. We have the baseline
  numbers in `bench/`.

Scenarios #2 (no-stutter UX), #5 (always-on parent), and #6
(time-travel debug) are second-wave content — each is a separate
blog post / launch moment after v0.4's first wave.

## See also

- [`DESIGN-v0.4.md`](./DESIGN-v0.4.md) — the technical RFC.
- [`DESIGN-v0.4-PHASE3-SPIKE.md`](./DESIGN-v0.4-PHASE3-SPIKE.md) —
  integration options (FC vmstate-only, tmpfs discard, raw KVM bypass).
- [`bench/pause-window/RESULTS-v0.3.md`](./bench/pause-window/RESULTS-v0.3.md)
  — v0.3.4 pause-window data (the baseline v0.4 improves on).
- [`experiments/v0.4-uffd-wp-poc/RESULTS.md`](./experiments/v0.4-uffd-wp-poc/RESULTS.md)
  — empirical proof of the ~3 ms/GiB scaling claim.
