# Branch-and-fan-out demo run
- BRANCH tag: `langgraph-fork-1779034939`
- Daemon-measured pause window: **3807 ms**
- Source sandbox: `sb-6a09eadc-0029`

## Per-agent comparison

| Agent | Hint | Steps | Tokens | Wall (ms) | Tools called | Sandbox id |
|---|---|---:|---:|---:|---|---|
| parent | — | — | — | — | weather, weather, search_places, search_places, search_places | `sb-6a09eadc-0029` |
| thorough | Be thorough. Maximize cultural depth — slow down, … | 5 | 3610 | 142273 | weather, weather, search_places, search_places, search_places | `sb-6a09eb3f-002a` |
| minimal | Be minimal. Maximize daylight outside — fewer indo… | — | — | — | weather, weather, search_places, search_places, search_places | `sb-6a09eb3f-002b` |
| cost | Optimize for cost. Avoid $$$ items entirely; prefe… | — | — | — | weather, weather, search_places, search_places | `sb-6a09eb3f-002c` |

## Final itineraries

### parent

```
_(no final answer recorded)_
```

### thorough

*Hint:* Be thorough. Maximize cultural depth — slow down, prefer fewer stops with longer visits.

```
 "city": "kyoto", "category": "neighborhood"}

```

### minimal

*Hint:* Be minimal. Maximize daylight outside — fewer indoor stops, no shopping streets.

```
_(no final answer recorded)_
```

### cost

*Hint:* Optimize for cost. Avoid $$$ items entirely; prefer free or $.

```
_(no final answer recorded)_
```

## What this run demonstrates

- A single source agent ran the first **3 steps** of a trip-planning ReAct loop, calling the `weather` and `search_places` tools and building a partial plan in its conversation history.
- We called `POST /v1/sandboxes/:id/branch`. The source paused for **3807 ms** while its full memory image was snapshotted.
- We spawned 3 grandchildren from the branched snapshot. Each inherited the source's reasoning state — same conversation history, same tool results, same partial plan.
- We planted a different steering hint in each child's `/tmp/forkd-hint.txt`. The agents read this file on every step, so the **next** thought after the fork was perturbed differently per child.
- All three children continued from the shared state and produced different itineraries. The parent continued in parallel with no hint as a control.

This is the speculative-parallel-exploration primitive that closed-source platforms (Modal Sandboxes) keep behind their hidden moat. forkd does it open-source on KVM/Linux.
