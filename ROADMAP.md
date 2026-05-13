# Roadmap

forkd's planning horizon. Items are grouped into milestones; each
milestone ends with a deliberate marketing / community pulse so the
next milestone's priorities are driven by real user feedback rather
than guesses.

Tracked work for individual items lives in
[GitHub issues](https://github.com/deeplethe/forkd/issues).

## M1 — Widen the entry point (≈4 weeks)

The bottleneck right now is not raw performance — it's that the
project speaks only to Python / numpy workloads, and the first-time
setup is a ~10 minute rootfs build. Both are fixable inside one
milestone.

### M1.1 — `recipes/playwright-browser/`  (≈1 week)

Browser fan-out is the second-largest AI-agent workload shape after
Python (Anthropic computer-use, OpenAI browsing, every coding-agent
that uses Playwright/Puppeteer). Cold-start of a headless Chromium
container is 2–3 s; fork-from-warm should put it at ~10 ms — a
100–300× win on a load that the project doesn't currently serve at
all.

Done when:

- ✓ Recipe builds a parent rootfs from the official Playwright image
  with a headless Chromium already running in the parent.
  ([scaffold](./recipes/playwright-browser/))
- ✓ `forkd-agent` ships a JS bridge: `sb.eval(<js>)` against any
  child routes to the warmed Chromium via the warmup subprocess.
  ([#30](https://github.com/deeplethe/forkd/issues/30))
- ☐ `forkd fork --tag browser -n 50 --per-child-netns` clean spawns
  on Firecracker (end-to-end run pending).
- ☐ Benchmark numbers in `bench/` and a row in the README recipe
  table.

Risk: Chromium internal timers / GPU init may behave oddly across
snapshot-restore. Budget +2 days if so.

### M1.2 — Snapshot Hub MVP  (≈1.5 weeks)

Right now `forkd run` requires a ~10 minute first-time rootfs build
and warmup per recipe. A public registry of pre-built parent
snapshots (`memory.bin` + `rootfs.ext4` + `vmstate.json` + manifest,
hosted as OCI artifacts on a bucket) collapses that to a single
`forkd pull` (~30 s).

Done when:

- CLI ships `forkd pull <owner>/<tag>` and `forkd push <owner>/<tag>`.
- Snapshot pack format documented (tar.zstd + manifest.toml).
- All 6 existing recipes pushed to the registry; on a clean Ubuntu
  host, `forkd pull deeplethe/python-numpy && forkd fork --tag
  python-numpy -n 100` works end-to-end without running any recipe
  build script.
- README quickstart rewritten to lead with `forkd pull`.

Risk: base-image redistribution licensing per recipe. Triaged 1-time
at start; expected clear since all our base images are Apache / BSD /
MIT.

### M1.3 — Marketing pulse  (≈1 week)

The week M1.1 and M1.2 are done, before starting M2: ship one
substantial English blog post ("Forking Playwright at 10 ms"), Show
HN, and equivalents on r/MachineLearning, r/LocalLLaMA, XHS / 知乎.
The point is to **collect external signal** that determines whether
M2 priorities still make sense.

## M2 — Depth, driven by M1 feedback (≈8 weeks)

The two items below are pre-selected as M2 candidates, but their
order and scope **must be re-confirmed** after the M1.3 marketing
pulse — if real users surface a more urgent gap, that takes priority.

### M2.1 — Differential snapshots  (≈3 weeks)

Today, modifying a parent (e.g. `pip install` a new package) means
re-running the whole snapshot pipeline. Firecracker already supports
`snapshot_type: "Diff"`; we just need to surface it as a chain
(`base → +pandas → +sklearn`) at the controller / CLI / registry
layer. Dev iteration drops from ~10 s + several-GB I/O to seconds.

Done when:

- `forkd snapshot diff --from <tag> --tag <new-tag>` produces a
  diff < 100 MB for an `apt install` or `pip install` delta.
- Restore time on a 3-snapshot chain is within 10% of the base
  snapshot's restore time.
- Snapshot Hub MVP (M1.2) understands chains: pulling a diff also
  pulls its parents.

Risk: Firecracker diff-snapshot has some vmstate fields that don't
chain cleanly. Highest-risk item in the roadmap; budget +1 week if
the first 2 weeks hit an edge case.

### M2.2 — Time-travel branching execution  (≈4–5 weeks)

Allow a workload to pause at an arbitrary execution point and fork
into K children, each with a variant of some input (env var, random
seed, prompt). This is the primitive that AI-agent researchers
actually want — tree search over agent decision space — and no
competing project offers it.

Done when:

- `state.fork(k=5, vary={"temperature": [...]})` exposed in the
  Python SDK.
- One end-to-end demo: an LLM agent forks at a tool-call decision
  point into 5 branches, returns the highest-scoring continuation.
- Demo gif + blog post + HN-ready.

Risk: this is a product project, not a pure engineering one. If the
demo doesn't sing, the work is wasted. Should only be greenlit if
M1.3 produces an audience to demo to.

## M3 — Production-readiness (≈ open-ended)

Tracked from `README.md#Status`. Not committed to a date; expected
to be unblocked by paying or prospective enterprise users surfacing
specific gaps.

- Default-deny egress per child netns.
- `cpu.max`, `io.max`, `pids.max` quotas beyond the existing
  `memory.max`.
- TLS termination in the daemon (currently rely on reverse proxy).
- Multi-node scheduling (one daemon → multiple hosts).
- Third-party security audit.
