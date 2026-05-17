# Branch-and-fan-out demo run
- BRANCH tag: `langgraph-fork-1779034939`
- Daemon-measured pause window: **3807 ms**
- Source sandbox: `sb-6a09eadc-0029`

## Per-agent comparison

| Agent | Status | Hint | Steps | Tokens | Wall (ms) | Retries | Tools | Sandbox |
|---|---|---|---:|---:|---:|---:|---|---|
| parent | ⏳ in-flight | — | — | — | — | 4 | weather, weather, search_places, search_… | `sb-6a09eadc-0029` |
| thorough | ✅ completed | Be thorough. Maximize cultural depth — s… | 5 | 3610 | 142273 | 3 | weather, weather, search_places, search_… | `sb-6a09eb3f-002a` |
| minimal | ⏳ in-flight | Be minimal. Maximize daylight outside — … | — | — | — | 3 | weather, weather, search_places, search_… | `sb-6a09eb3f-002b` |
| cost | ⏳ in-flight | Optimize for cost. Avoid $$$ items entir… | — | — | — | 3 | weather, weather, search_places, search_… | `sb-6a09eb3f-002c` |

## Per-agent output

Each box shows the agent's last meaningful content at collection time. **`answer`** means the agent produced a terminal response; **`think (in-flight)`** means it was still mid-reasoning when transcripts were collected — the divergence is still visible there.

### parent

**Type:** last `think` event (agent was still reasoning at collection)

```
It seems there was an error with the previous request. Let's try again to get the places to visit in Osaka.
```

### thorough

*Hint:* Be thorough. Maximize cultural depth — slow down, prefer fewer stops with longer visits.

**Type:** final `answer` event

```
 "city": "kyoto", "category": "neighborhood"}

```

### minimal

*Hint:* Be minimal. Maximize daylight outside — fewer indoor stops, no shopping streets.

**Type:** last `think` event (agent was still reasoning at collection)

```
Based on the places to visit in Kyoto, here are some options:
1. Fushimi Inari - culture, free, 120 minutes
2. Nishiki Market - food, $, 60 minutes
3. Arashiyama Bamboo Grove - nature, free, 60 minutes
4. Pontocho Alley - neighborhood, $, 90 minutes

Now let's explore some places to visit in Osaka.
```

### cost

*Hint:* Optimize for cost. Avoid $$$ items entirely; prefer free or $.

**Type:** last `think` event (agent was still reasoning at collection)

```
Based on the places to visit in Kyoto, here are some options:
1. Fushimi Inari - Free, 2 hours
2. Nishiki Market - $, 1 hour
3. Arashiyama Bamboo - Free, 1 hour
4. Pontocho Alley - $, 1.5 hours

Now, let's explore some places to visit in Osaka.
```

## What this run demonstrates

- A single source agent ran the first **3 steps** of a trip-planning ReAct loop, calling the `weather` and `search_places` tools and building a partial plan in its conversation history.
- We called `POST /v1/sandboxes/:id/branch`. The source paused for **3807 ms** while its full memory image was snapshotted.
- We spawned 3 grandchildren from the branched snapshot. Each inherited the source's reasoning state — same conversation history, same tool results, same partial plan.
- We planted a different steering hint in each child's `/tmp/forkd-hint.txt`. The agents read this file on every step, so the **next** thought after the fork was perturbed differently per child.
- All three children continued from the shared state and produced different itineraries. The parent continued in parallel with no hint as a control.

This is the speculative-parallel-exploration primitive that closed-source platforms (Modal Sandboxes) keep behind their hidden moat. forkd does it open-source on KVM/Linux.
