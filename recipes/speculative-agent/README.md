# `speculative-agent`

The headline forkd recipe. An agent reaches a decision point, BRANCHes
its sandbox, fans out N children that each pursue a different
strategy, and a judge picks the best one. Losers are discarded — but
they all ran in parallel, from one warmed parent, in ~200ms.

The other four recipes ([`mcp-agent/`](../mcp-agent/),
[`crewai-fanout/`](../crewai-fanout/),
[`autogen-branch/`](../autogen-branch/),
[`openai-swarm/`](../openai-swarm/)) demonstrated forkd inside a
specific framework's idiom. This recipe shows the **pattern** —
speculative execution as a decision primitive — that BRANCH enables
and nothing else open-source does today.

## The pitch

```
          ┌──────────────────────────────────┐
          │  source sandbox — agent at the   │
          │  decision point (question loaded,│
          │  context warm, libraries imported)│
          └────────────────┬─────────────────┘
                           │   forkd BRANCH (diff=true, ~200ms)
                  ┌────────┴────────┐
                  ▼        ▼        ▼
              ┌──────┐ ┌──────┐ ┌──────┐
              │ loop │ │formula│ │numpy │
              │      │ │      │ │      │
              │ 12ms │ │1.5µs │ │450µs │   ← actual run times
              └──┬───┘ └──┬───┘ └──┬───┘     each in its own microVM
                 └────────┼────────┘         each inherits warm state
                          ▼
                     judge → "formula"
                     8000× faster than loop
```

Three strategies, one problem. The winner is decided in milliseconds.
The losers are killed. This is the speculative-decoding pattern from
LLM serving, applied to agent decisions instead of token sampling.

## What this script does

1. Provisions one source sandbox.
2. **BRANCH** the source with `diff=true`. v0.3 fast path: ~200ms
   pause window, captures the source's full memory + execution state.
3. Spawns N grandchildren from the branch (one per strategy). Each
   inherits the source's warm Python state — no re-import, no
   re-warmup tax.
4. Each grandchild runs a different strategy for the SAME problem
   (`sum of squares 1..N`). Strategies all produce the same answer
   but have wildly different wall times.
5. A judge picks the fastest correct answer. Prints which strategy
   won, all answers (correctness check), and the speedup vs the
   slowest.

## Setup

1. **forkd-controller running** with a Python+numpy snapshot.

2. **Per-child netns** for the fanout (N=3 default):
   ```bash
   sudo bash scripts/host-tap.sh
   sudo bash scripts/netns-setup.sh 3
   ```

3. **Install** (~30 seconds):
   ```bash
   pip install forkd>=0.3.2
   ```

4. **Run:**
   ```bash
   FORKD_TOKEN=$(sudo cat /etc/forkd/token) \
     python3 recipes/speculative-agent/demo.py --n=3
   ```

No LLM key required. The "agent" is deterministic Python imitating
the speculative-execution pattern an LLM agent would use; the LLM
piece is what would choose the strategies — forkd is the substrate
that makes trying all of them cheap.

## Expected output

```
[speculative] using snapshot 'coding-agent-fork-prewarm-v1'
[speculative] source sandbox: sb-...-0001
[speculative] BRANCH (diff=true) in 287ms (diff_physical_bytes=393216)
[speculative] spawned 3 grandchildren in 95ms (32ms/child)
  [loop    ] sb-...-0002 → answer=333338333350000 wall_us=12041
  [formula ] sb-...-0003 → answer=333338333350000 wall_us=2
  [numpy   ] sb-...-0004 → answer=333338333350000 wall_us=489

[speculative] WINNER: formula (2 µs)
[speculative] winner is 6020.5× faster than the slowest strategy
[speculative] cleaned up 3 grandchildren
[speculative] cleaned up source sandbox sb-...-0001
```

The actual numbers vary with snapshot warmup and host load, but the
shape is the same: BRANCH is ~200-500ms, fanout is ~30ms/child, and
the strategies differ by 2-4 orders of magnitude in compute. **The
speculation found out for you which strategy actually wins on this
problem.**

## Why this is the BRANCH-shaped recipe

Three properties that no other open-source primitive gives you:

1. **Warm state inheritance.** Every grandchild inherits the source's
   imports, scratch state, env vars. Docker can't — it cold-starts.
   bare-metal threads can — but they share state catastrophically.

2. **Real isolation between strategies.** Each strategy runs in its
   own KVM-backed microVM. A runaway loop, an `os.exit(1)`, an OOM
   in one strategy doesn't poison the others.

3. **Sub-second decision latency.** From the decision point to the
   judge's pick: BRANCH (200ms) + spawn (~30ms × N) + the longest
   strategy. The 8000× speedup the demo prints is the agent
   noticing "formula is the right tool" — without paying for the
   loop on every future invocation.

## Adapting to your own agent

The pattern is general:

```python
# 1) at a decision point in your agent (LangGraph node, AutoGen
#    turn handler, CrewAI task, custom REPL...):
branch = ctrl.branch_sandbox(current_sandbox_id, diff=True)

# 2) fan out N candidates. each gets a slightly different "hint"
#    or "strategy" — could be a different prompt, different tool
#    choice, different code path.
children = ctrl.spawn_sandboxes(branch["tag"], n=N, per_child_netns=True)

# 3) run each candidate (LLM call, code exec, browse, whatever).
results = [run_strategy(c.id, strategy_hint[i]) for i, c in enumerate(children)]

# 4) judge — could be an LLM judge, a deterministic ranker, a unit
#    test, an objective like wall_us, anything.
winner = max(results, key=score)

# 5) keep the winner's sandbox alive as the new source. kill the
#    others. continue.
for r in results:
    if r is not winner:
        ctrl.kill_sandbox(r.sandbox_id)
```

That's the entire speculative-agent pattern. The rest is choosing
good strategies (your LLM does that) and choosing a good judge
(your domain knowledge).

## Rotate the source after N BRANCHes ([#146](https://github.com/deeplethe/forkd/issues/146))

Doing more than 2-3 BRANCHes on the **same source sandbox** triggers a
known anomaly: `pause_ms` typically grows ~5× from BRANCH 3 onward
(280 ms → 1.3-1.5 s). One-shot speculative-agent (this recipe) only
takes one BRANCH per source, so it doesn't hit this. But a loop that
keeps BRANCHing the same agent across many decision points will.

forkd v0.3.3+ surfaces the warning two ways:

- `forkd ls` shows a **BRANCHES** column; counts ≥3 are highlighted
- the BRANCH response includes a `warning` field (`SnapshotInfo.warning`)

The fix is mechanical: after every N BRANCHes (N = 2 is conservative,
3 is the threshold), kill the source sandbox and spawn a fresh one
from the latest BRANCH. The new sandbox has `branch_count = 0` and
the chain continues at full speed.

```python
ROTATE_EVERY = 2
branches_done = 0

while still_thinking:
    branch = ctrl.branch_sandbox(source_id, diff=True)
    # ... fan out, judge, winner = ...

    branches_done += 1
    if branches_done >= ROTATE_EVERY:
        # Recycle the source. The latest BRANCH carries the full state;
        # spawning from it gives us a fresh sandbox with branch_count=0.
        ctrl.kill_sandbox(source_id)
        [new_source] = ctrl.spawn_sandboxes(
            snapshot_tag=branch["tag"], n=1, per_child_netns=True
        )
        source_id = new_source["id"]
        branches_done = 0
```

Aggregate downtime is still 14× better than Full BRANCHes even
without rotation, so this is a polish move, not a correctness one.
Track [#146](https://github.com/deeplethe/forkd/issues/146) for the
upstream fix.

## Troubleshooting

- **`numpy` import fails in the numpy strategy** → the rootfs doesn't
  have numpy. Use `recipes/python-numpy/` to build one, or drop the
  numpy strategy with `--n=2`.
- **Loop strategy times out** → bump `N_PROBLEM` down in `demo.py`,
  or raise `timeout_secs=` in the `exec_command` call.
- **All strategies report the same wall_us** → snapshot was warmed
  too well; the timing differences live below the wall-clock noise
  floor for that N. Increase `N_PROBLEM` to amplify.

## See also

- [`mcp-agent/`](../mcp-agent/), [`crewai-fanout/`](../crewai-fanout/),
  [`autogen-branch/`](../autogen-branch/),
  [`openai-swarm/`](../openai-swarm/) — same primitive, different
  framework idioms.
- [`langgraph-react/`](../langgraph-react/) — full rootfs build + a
  real ReAct agent that BRANCHes mid-thought.
- [`bench/pause-window/RESULTS-v0.3.md`](../../bench/pause-window/RESULTS-v0.3.md)
  — the v0.3 numbers behind the ~200ms BRANCH.
