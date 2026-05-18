# Branch-and-fan-out demo — real results

The forkd "fork a thinking agent" demo, end-to-end on real
hardware. The latest clean run is in
[`results-2026-05-18/`](./results-2026-05-18/); the earlier
[`results-2026-05-17/`](./results-2026-05-17/) is the same
mechanism with a less-capable model (Qwen2.5-7B) — kept for
comparison so you can see what changes when you swap models.

## TL;DR for a tweet thread

> 🍴 forkd just forked a running ReAct agent: **163 ms** pause on tmpfs-backed snapshot storage, **4 s** on the SATA SSD this demo recorded against. Same code, only the disk differs.
>
> A source agent had spent 2 steps gathering weather + place
> data for a Kyoto + Osaka trip. We BRANCHed it and spawned 3
> grandchildren from the same cognitive state. Each got a
> different steering hint — "be thorough", "be minimal",
> "optimize for cost".
>
> All 3 produced **different** itineraries, inheriting the same
> tool results, same conversation history, same Python heap.
> The only thing that diverged was the next thought.
>
> Headline divergence: the parent (no hint) put Nishiki Market
> on Day 1. All three hinted children dropped it and substituted
> Arashiyama Bamboo Grove — a free outdoor activity. The
> cost-focused child even annotated dining stops with "may be
> pricey" warnings.
>
> This is the speculative-parallel-exploration primitive Modal
> Sandboxes keeps closed-source. Now on KVM, open-source. ↓

## The setup that produced the run

- Host: yangdongxu-desktop, Ubuntu 24.04, Linux 6.14, 20 vCPU, 30 GiB RAM
- forkd built from `demo/summary-show-in-flight` (see PR #66)
- Source rootfs: `python:3.12-slim` + `requests`, ~206 MiB
- LLM: **DeepSeek-V3** via SiliconFlow's OpenAI-compatible API
- Task: "Plan a 2-day trip to Kyoto and Osaka. Use the tools to check weather and find places."

## Headline numbers

| Metric | Value |
|---|---|
| Daemon-measured pause window | **4007 ms** (SATA SSD storage; see [RESULTS-v0.2.md](../../bench/pause-window/RESULTS-v0.2.md) for 163 ms on tmpfs) |
| Memory image size | 513 MiB |
| Grandchildren spawned | 3 |
| Steering hints applied | 3 (one per child) |
| Network retries this run | **0** (clean) |
| Per-agent token cost | 1395–1546 |
| Snapshot tag (auditable) | `langgraph-fork-1779037370` |

## The divergence at a glance

| Agent | Hint | Day-1 afternoon (Kyoto) | Notable framing |
|---|---|---|---|
| **parent** | _(none — control)_ | **Nishiki Market** ($$) | baseline; no special framing |
| **thorough** | "cultural depth, slow" | **Arashiyama Bamboo** (free) | replaced shopping w/ cultural-nature |
| **minimal** | "daylight outside, no shopping" | **Arashiyama Bamboo** (free) | replaced shopping w/ outdoor |
| **cost** | "avoid \$\$\$, prefer free or \$" | **Arashiyama Bamboo** (free) | + warning labels on $$ stops, explicit cost-optimization footer |

Worth highlighting: the model wasn't told to "drop Nishiki Market" or "add Arashiyama". It chose to re-rank based on the hint. All three hinted children **independently agreed** on the substitution. Cost went further and added meta-commentary like "though dining options may be pricey" and an explicit "Cost Optimization" footer that the others didn't.

## Full itineraries

See [`results-2026-05-18/summary.md`](./results-2026-05-18/summary.md) for the auto-generated render of all four agents' final answers. Raw per-event JSONL is in the same directory.

## What this validates

1. **The BRANCH primitive works on a real agent workload.** 4 s pause, 0 errors, all 4 agents completed cleanly with their respective post-branch reasoning.
2. **In-guest agents are pause-blind.** No socket errors, no timeouts at wake-up, no retries needed in this run. Same pattern we measured synthetically in [`bench/pause-window/RESULTS-v0.2.md`](../../bench/pause-window/RESULTS-v0.2.md), now confirmed on a real LLM agent.
3. **Hint-based perturbation post-branch is real.** Each child's NEXT LLM call sees a different system message; the inherited conversation history + tool results stay the same. This is the cheapest faithful model of speculative parallel exploration on a stateful agent.

## What the earlier run 9 shows (and what we learned from it)

The first end-to-end run (committed in [`results-2026-05-17/`](./results-2026-05-17/)) used Qwen2.5-7B-Instruct. The mechanism worked but the model:
- Had network retries on first call after restore (~90 s wall before reaching branch)
- Occasionally emitted tool-call arguments as freeform content
- Kept calling search_places past the point where it should have produced a final answer

The hint side-channel STILL worked — the children's in-flight `think` events showed clear divergence (e.g. minimal's "Nishiki Market - food, $" vs the original "food, $$" — model self-downgraded the price). But the answers came out messy.

The fix landed in PR #66:
1. Default model bumped to DeepSeek-V3 (much better tool discipline)
2. System prompt explicit about "use each tool at most twice, then stop calling tools"
3. `branch_after_step=2` (DeepSeek converges in 2 steps; the prior `=3` was unreachable)
4. `summarize.py` falls back to last `think` when no `answer` exists, so future flaky runs still tell a story

run-12 (2026-05-18) reflects all of those. Same mechanism, cleaner output.

## Reproducing

```bash
export FORKD_URL=http://127.0.0.1:8889
export FORKD_TOKEN=$(cat /etc/forkd/token)
export SILICONFLOW_API_KEY=...
bash recipes/langgraph-react/demo.sh
```

`recipes/langgraph-react/README.md` has the detailed recipe + design notes.
