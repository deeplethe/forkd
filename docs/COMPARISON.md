# Compute substrates for AI agents: a landscape note

How forkd positions against the other open-source (and one closed)
projects in the AI-agent compute space. Written so a reader can pick
between them by use case, not as a competitive teardown.

All four projects below use KVM for hardware isolation. The
architectural choices are different and reflect different priorities,
not different opinions about what "right" looks like.

## At a glance

| Project | Architecture | Fork-on-write | Multi-node scheduling | Persistent state model | Stars | License |
|---|---|:---:|:---:|---|---:|---|
| [Modal](https://modal.com) | Closed SaaS | yes (closed) | yes | sandbox lifecycle | n/a | proprietary |
| [CubeSandbox](https://github.com/TencentCloud/CubeSandbox) | Daemon + cluster (custom VMM on rust-vmm) | roadmap | yes (CubeMaster/Cubelet/CubeNet) | template clone pool | 5.7k | Apache 2.0 |
| [boxlite](https://github.com/boxlite-ai/boxlite) | Library (Rust core + multi-lang SDKs) | no | no | stateful boxes that survive stop/restart | 2.1k | Apache 2.0 |
| [forkd](https://github.com/deeplethe/forkd) | Daemon (Firecracker) | **yes** | no (single-host today) | snapshot tag + branch | <100 | Apache 2.0 |

## Modal (proprietary)

The reference data point. Modal pioneered hosting AI-agent sandboxes
as a cloud service. They run fast cold starts, support a Sandbox
fork primitive, and have years of production. The trade-off is
closed source and a SaaS-only deployment model. If you're building
a product that can sit on top of Modal, the question is whether the
fork primitive (or rebuild-on-different-tradeoffs cost) is worth it.

## CubeSandbox

Tencent's open-source sandbox runtime. Built on rust-vmm crates
into a custom, aggressively trimmed VMM. Optimized for two
properties: **<60 ms cold start** (via pool pre-provisioning) and
**<5 MB per-instance memory overhead** (via CoW + trimmed runtime).
Ships with a CubeMaster / Cubelet / CubeNet cluster architecture
out of the box.

Fork-on-write is on their roadmap as "Event-level snapshot rollback
(coming soon)" but not shipped. Pause and resume endpoints exist;
fork-from-snapshot is the missing piece.

If you need to scale across many physical nodes today and your
agents are E2B-compatible, this is the most direct OSS answer.

See [`INTEGRATION-CUBESANDBOX.md`](./INTEGRATION-CUBESANDBOX.md) for
a deeper compare of snapshot internals.

## boxlite

A compute substrate built as a library, not a daemon. You
`pip install boxlite` (or the equivalent in Node, Rust, Go, CLI)
and your code spawns lightweight VMs ("Boxes") that run standard
OCI containers inside them. No daemon to manage, no root required
on the host's part, multi-language SDKs out of the box.

Their core differentiator is **stateful boxes**. A Box can stop,
restart, and pick up where it left off with packages and files
intact. This targets the "I'm building an agent that wants a real
workspace it can come back to" use case rather than the "I want
N sandboxes for one-shot tasks" use case.

boxlite does not currently offer a fork-on-write primitive. If
your workload is one stateful agent per session rather than
fan-out, this is the project to look at first.

In our N=100 fan-out benchmark
([`bench/BOXLITE.md`](../bench/BOXLITE.md)), boxlite takes about
113 seconds total wall-clock to spawn 100 boxes that each `import
numpy`. forkd takes about 1.06 seconds for the same workload. The
gap reflects optimization target: boxlite is designed for the
stateful single-session case, forkd for the fan-out-from-warmed-
parent case. Neither number is wrong for what the project is
trying to do.

## forkd (this project)

A focused fork-on-write primitive for microVMs. forkd's `POST
/v1/sandboxes/:id/branch` pauses a running source VM, writes a
snapshot, resumes the source, and lets N children spawn from
that snapshot sharing memory copy-on-write. This is the
primitive Modal keeps proprietary; forkd is the open-source
equivalent.

The trade-off is everything that's not the fork primitive.
forkd is single-host today, has a daemon model rather than
library, ships a Python SDK and a CLI but not the multi-language
spread boxlite has, and ships with fewer recipes than
CubeSandbox's polished sandbox templates.

Pause window measurement
([`bench/pause-window/RESULTS-v0.2.md`](../bench/pause-window/RESULTS-v0.2.md)):
163 ms ± 7 ms on tmpfs-backed snapshot storage, 4.26 s ± 0.41 s
on SATA SSD. Same code; the difference is the disk fsync
throughput of the snapshot write path.

## When to use which

| Your situation | Best fit |
|---|---|
| You want a managed service with SLA and don't care about open source | Modal |
| You're at "thousands of concurrent sandboxes on a single node" scale with cluster requirements | CubeSandbox |
| You want a library that embeds into your existing Python / Node app with zero infrastructure | boxlite |
| You want one stateful workspace per user that survives across sessions | boxlite |
| You want to fork a running stateful agent into N parallel branches | forkd |
| You're at the experimentation phase and want to swap backends without code changes | Any of the three OSS options behind the E2B SDK |

The four projects overlap less than the framing of "AI agent
sandbox space" suggests. Most production workloads would pick
exactly one of them. Recipes that mix two are a v0.3 follow-up
across the ecosystem.

## What forkd specifically does that the others don't (yet)

The fork-on-write primitive is the headline. Concretely:

- `POST /v1/sandboxes/:id/branch` on a running VM produces a
  new tagged snapshot in ~163 ms (tmpfs) to ~4 s (SSD).
- Three or more child sandboxes can then be spawned from the
  snapshot, each inheriting the source's address space CoW.
- We've measured that **in-guest agents are nearly pause-blind**
  during the BRANCH operation: connection survival 5/5,
  in-flight loss 0/5 across the trials. The pain of the pause
  is on external observers, not on the agent itself.

For a worked example, see the langgraph-react demo
([`recipes/langgraph-react/DEMO.md`](../recipes/langgraph-react/DEMO.md))
where a ReAct agent is BRANCHed mid-thought and three
grandchildren each take a different steering hint, producing
visibly different reasoning paths from the same prior state.

## Beyond the four-way

[E2B](https://e2b.dev) is the API spec all three OSS projects
gesture at. CubeSandbox is an E2B drop-in. forkd's Python SDK
matches E2B's surface. boxlite's `SimpleBox` reads similar to
E2B's surface. If your agent uses the E2B SDK, you can switch
backends with one environment variable; the fork primitive is
the only capability unique to forkd in this mix.

Other projects we've benchmarked (gVisor, plain Firecracker,
fresh Docker, OpenSandbox) live further afield and are covered
in [`bench/README.md`](../bench/README.md).

## Calibration note

This page tries to be calibrated rather than promotional. forkd
is the smallest of the four projects by stars and by team size.
Modal has years of production, CubeSandbox has Tencent's
engineering, boxlite has a polished library-first DX and
funded-startup velocity. forkd's contribution to the ecosystem
is the open-source fork primitive, not a complete substitute
for what any of the other three does well.

If you find an error in any of the above (especially in the
descriptions of Modal, CubeSandbox, or boxlite), please open an
issue. We'd rather be corrected than misleading.
