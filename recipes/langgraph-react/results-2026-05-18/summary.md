# Branch-and-fan-out demo run
- BRANCH tag: `langgraph-fork-1779037370`
- Daemon-measured pause window: **4007 ms**
- Source sandbox: `sb-6a09f4ae-0035`

## Per-agent comparison

| Agent | Status | Hint | Steps | Tokens | Wall (ms) | Retries | Tools | Sandbox |
|---|---|---|---:|---:|---:|---:|---|---|
| parent | ✅ completed | — | 2 | 1470 | 69254 | 0 | weather, weather, search_places, search_… | `sb-6a09f4ae-0035` |
| thorough | ✅ completed | Be thorough. Maximize cultural depth — s… | 2 | 1481 | 71994 | 0 | weather, weather, search_places, search_… | `sb-6a09f4be-0036` |
| minimal | ✅ completed | Be minimal. Maximize daylight outside — … | 2 | 1449 | 67940 | 0 | weather, weather, search_places, search_… | `sb-6a09f4be-0037` |
| cost | ✅ completed | Optimize for cost. Avoid $$$ items entir… | 2 | 1546 | 79947 | 0 | weather, weather, search_places, search_… | `sb-6a09f4be-0038` |

## Per-agent output

Each box shows the agent's last meaningful content at collection time. **`answer`** means the agent produced a terminal response; **`think (in-flight)`** means it was still mid-reasoning when transcripts were collected — the divergence is still visible there.

### parent

**Type:** final `answer` event

```
### Day 1: Kyoto
- **Morning**: Fushimi Inari (120 min, free)  
  Explore the iconic red torii gates and enjoy the serene atmosphere.
- **Afternoon**: Nishiki Market (60 min, $$)  
  Sample local delicacies and shop for unique souvenirs.
- **Evening**: Pontocho Alley (90 min, $$)  
  Stroll through this charming alleyway and dine at one of its traditional restaurants.

### Day 2: Osaka
- **Morning**: Osaka Castle (60 min, $)  
  Visit this historic landmark and learn about Japan's feudal history.
- **Afternoon**: Kuromon Market (60 min, $$)  
  Indulge in fresh seafood and other local treats.
- **Evening**: Dotonbori (120 min, $$)  
  Experience the vibrant nightlife and neon lights of Osaka's entertainment district.

Both days will have pleasant weather, with sunny skies and no rain. Enjoy your trip!
```

### thorough

*Hint:* Be thorough. Maximize cultural depth — slow down, prefer fewer stops with longer visits.

**Type:** final `answer` event

```
### Day 1: Kyoto
- **Morning**: Fushimi Inari (120 min, free) — Explore the iconic torii gates and immerse yourself in the spiritual atmosphere.
- **Afternoon**: Arashiyama Bamboo (60 min, free) — Stroll through the serene bamboo forest and enjoy the natural beauty.
- **Evening**: Pontocho Alley (90 min, $$) — Wander through this historic alleyway and savor Kyoto's culinary delights.

### Day 2: Osaka
- **Morning**: Osaka Castle (60 min, $) — Discover the history and grandeur of this famous landmark.
- **Afternoon**: Kuromon Market (60 min, $$) — Indulge in fresh seafood and local street food.
- **Evening**: Dotonbori (120 min, $$) — Experience the vibrant nightlife and neon lights of Osaka's entertainment district.

Both days will be sunny and warm, perfect for exploring!
```

### minimal

*Hint:* Be minimal. Maximize daylight outside — fewer indoor stops, no shopping streets.

**Type:** final `answer` event

```
### Day 1: Kyoto
- **Morning**: Fushimi Inari (120 min, free) — Explore the iconic torii gates and enjoy the sunny weather.
- **Afternoon**: Arashiyama Bamboo (60 min, free) — Walk through the serene bamboo forest.
- **Evening**: Pontocho Alley (90 min, $$) — Stroll through this atmospheric alley and enjoy dinner.

### Day 2: Osaka
- **Morning**: Osaka Castle (60 min, $) — Visit this historic landmark in the warm sunshine.
- **Afternoon**: Kuromon Market (60 min, $$) — Sample local food and enjoy the lively atmosphere.
- **Evening**: Dotonbori (120 min, $$) — Experience the vibrant nightlife and delicious street food.
```

### cost

*Hint:* Optimize for cost. Avoid $$$ items entirely; prefer free or $.

**Type:** final `answer` event

```
### Day 1: Kyoto
- **Morning**: Fushimi Inari (120 min, free)  
  Explore the iconic torii gates and enjoy the serene atmosphere.
- **Afternoon**: Arashiyama Bamboo (60 min, free)  
  Walk through the stunning bamboo forest and take in the natural beauty.
- **Evening**: Pontocho Alley (90 min, $$)  
  Stroll through this charming alleyway, though dining options may be pricey.

### Day 2: Osaka
- **Morning**: Osaka Castle (60 min, $)  
  Visit this historic landmark and learn about its rich history.
- **Afternoon**: Kuromon Market (60 min, $$)  
  Experience the lively market scene, though food options may be on the pricier side.
- **Evening**: Dotonbori (120 min, $$)  
  Enjoy the vibrant nightlife and neon lights, but be mindful of costs.

**Weather Notes**:
- Kyoto: Sunny and dry, high of 26°C, low of 17°C.
- Osaka: Sunny and warm, high of 25°C, low of 19°C.  

**Cost Optimization**: Avoided $$$ items and prioritized free or $ options where possible.
```

## What this run demonstrates

- A single source agent ran the first **3 steps** of a trip-planning ReAct loop, calling the `weather` and `search_places` tools and building a partial plan in its conversation history.
- We called `POST /v1/sandboxes/:id/branch`. The source paused for **4007 ms** while its full memory image was snapshotted.
- We spawned 3 grandchildren from the branched snapshot. Each inherited the source's reasoning state — same conversation history, same tool results, same partial plan.
- We planted a different steering hint in each child's `/tmp/forkd-hint.txt`. The agents read this file on every step, so the **next** thought after the fork was perturbed differently per child.
- All three children continued from the shared state and produced different itineraries. The parent continued in parallel with no hint as a control.

This is the speculative-parallel-exploration primitive that closed-source platforms (Modal Sandboxes) keep behind their hidden moat. forkd does it open-source on KVM/Linux.
