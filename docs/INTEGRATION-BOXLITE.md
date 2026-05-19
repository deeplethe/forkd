# forkd × boxlite

How these two open-source projects relate technically and where
they can interoperate. Written for engineers from either team and
for operators considering deploying both.

This is a draft. It exists to structure a real conversation, not
to declare a partnership unilaterally.

## At a glance

| | forkd | [boxlite](https://github.com/boxlite-ai/boxlite) |
|---|---|---|
| Position | Fork-on-write microVM primitive | Embeddable compute substrate for AI agents |
| Deployment | Long-lived daemon (forkd-controller) | Library, no daemon required |
| VMM | Firecracker | libkrun (via `libkrun-sys`) |
| Hypervisor | KVM | KVM |
| API surface | REST `/v1/sandboxes/:id/branch`, Python `Controller`, `forkd` CLI | Python / Node / Rust / Go / CLI SDKs, single binary CLI installer |
| Cold start | ~150 ms (restore-many from snapshot) | seconds (cold libkrun boot + container start) |
| Fork-on-write | Shipped: `POST /branch` | Not in main; "snapshots" appears in author's personal repo description |
| Persistent state | Snapshot tag, durable across daemon restarts | Boxes survive stop/restart with packages and files intact |
| Multi-language SDK | Python only (plus CLI) | Python, Node.js, Rust, Go, CLI |
| Multi-node scheduling | Single-host today | Single-host today |
| License | Apache-2.0 | Apache-2.0 |

forkd and boxlite are complementary along the most natural axis: we
focus on the fork primitive, you focus on embeddable runtime with
broad language reach. The capability boxlite users would gain from
forkd (branch a running box into N parallel children) is exactly
what forkd does today. The capability forkd users would gain from
boxlite (no-daemon library mode, multi-language SDKs, stateful
single-session boxes) is exactly what boxlite does today.

## Where they overlap

Both projects make the same foundational decisions:

1. KVM for hardware isolation. Hardware isolation is non-negotiable
   for running untrusted AI-generated code; containers are not
   enough.
2. CoW memory at some level. boxlite uses libkrun's container-in-VM
   pattern; forkd uses Firecracker's snapshot-mmap pattern. Both
   end up sharing memory pages between sibling instances.
3. AI agent workloads as the target user. Both READMEs say so
   explicitly.
4. Apache 2.0. Reciprocal contribution is legally easy.

## Where they differ

### Deployment model

boxlite's thesis is "SQLite for compute": a library that embeds
directly into the application. No daemon, no root, no service to
manage. The cost is that lifecycle management belongs to the
embedding application; the benefit is zero infrastructure to
deploy.

forkd's choice is a long-lived daemon (`forkd-controller`). The
daemon owns VM lifecycle, snapshot storage, cgroup cleanup, and
the REST surface. The cost is that operators have one more
service to run; the benefit is centralized state management for
multi-tenant fork operations.

These aren't right-and-wrong choices. They're trade-offs for
different deployment shapes. A team building a developer-facing
desktop product probably wants boxlite's library model. A team
running shared sandbox infrastructure probably wants forkd's
daemon model.

### VMM choice

boxlite uses **libkrun** via `libkrun-sys`. libkrun is purpose-built
for embedding: minimal API surface, single-static-library
distribution, OCI container as the workload abstraction. It's the
correct choice for the library thesis.

forkd uses **Firecracker** as a subprocess. Firecracker has the
most production-tested microVM snapshot/restore in open source
today (years of running AWS Lambda + Fargate). It's the correct
choice for the fork primitive, where we lean heavily on the
snapshot/restore machinery.

Different VMM means different snapshot formats and different
ergonomics. We don't think anyone has tried to bridge these
specific two VMMs; if it's been done elsewhere we'd appreciate a
pointer.

### Fork-on-write

forkd ships `POST /v1/sandboxes/:id/branch` as the public
primitive: pause a running source VM, write a snapshot, resume
the source, spawn N children from the snapshot sharing memory
copy-on-write. The pause-window benchmark
([`bench/pause-window/RESULTS-v0.2.md`](../bench/pause-window/RESULTS-v0.2.md))
reports 163 ms ± 7 ms on tmpfs-backed snapshot storage and
4.26 s ± 0.41 s on SATA SSD for a 513 MiB source.

boxlite's public main branch does not currently expose a fork
primitive. The repo description on the project owner's personal
fork mentions "snapshots", which we read as a roadmap signal
rather than a shipped feature. The libkrun VMM itself does have
basic state save/restore in newer versions, but not the
"snapshot + spawn-N CoW" pattern that forkd exposes.

This is the gap the two projects could plausibly close together.

### Persistent state model

boxlite's stateful Boxes survive stop and restart with packages,
files, and environment intact. This targets the "I want a
workspace I can come back to" use case: long-running agent
sessions, shared workspaces, multi-day agent runs.

forkd's persistence model is the snapshot tag. Once a parent
snapshot is built, you can spawn N children from it across days
or weeks; the children are ephemeral by default but the parent
is durable. This targets the "I want a warmed template I can
fan out from cheaply" use case.

These models aren't mutually exclusive. A boxlite box that has
been built up over a session could plausibly be snapshotted (via
forkd-style primitive) when the user wants to fork it.

## Snapshot capability today

Comparing what each project ships right now:

| Feature | forkd | boxlite |
|---|:---:|:---:|
| Pause / resume a running VM | yes (Firecracker `PATCH /vm`) | yes (libkrun state controls) |
| Write a snapshot to disk | yes (vmstate.bin + memory.bin) | partial (libkrun supports state save in newer versions) |
| Restore from snapshot | yes (mmap MAP_PRIVATE) | partial |
| Spawn N children from one snapshot (CoW fan-out) | yes | not in public main |
| Application-driven BRANCH from a live, in-flight VM | yes (`POST /branch`) | not in public main |
| Hub registry for shareable snapshot packs | yes ([`docs/HUB.md`](./HUB.md)) | not applicable (library model ships images via OCI) |

The biggest single difference is the "spawn N children from one
snapshot, sharing CoW memory" capability. That's the primitive
this whole project is named after.

### What's in flight on the forkd side

**v0.3 phase 1 fully shipped (v0.3.0 + v0.3.1).** Diff-snapshot
BRANCH cuts source-pause window:

- 4 GiB SSD idle source: 29.3 s → 205 ms = **143×** (ceiling).
- 2 GiB SSD with 30-300 MiB dirty (typical agent workload): **6-15×**.
- 5 consecutive diff BRANCHes on the same sandbox (multi-BRANCH,
  v0.3.1): 4.7 s aggregate vs ~70 s Full = **14×**.

75+ trials of raw data in
[`bench/pause-window/`](../bench/pause-window/). Source TCP
connections, kvmclock, and timers see ~200 ms gaps instead of
seconds. v0.3.1 lifts the v0.3.0 first-BRANCH-only restriction by
chaining off the previous BRANCH's output — no separate shadow
file, zero extra storage.

Remaining v0.3 engineering wins, none of which require a Firecracker
fork:

- **NVMe + io_uring snapshot writer** — daemon flag for memory.bin
  writes. Expected SSD 10×+ on the underlying full-copy backbone
  (still bottlenecks total BRANCH API latency even with Diff mode).
- **Pre-emptive background snapshot** — background thread flushes
  dirty pages on a 1 s tick; at BRANCH, only flush what's dirty
  since the last tick. Bounds pause-window regardless of source
  size.
- **Cross-system benchmark** — same hardware, same recipes, forkd
  v0.3 + boxlite (when shipped) + CubeSandbox + Modal-style
  baseline. The 6-143× spread deserves a head-to-head story.

The original v0.3 plan was a memfd + uffd_wp live-fork architecture
targeting ~30 ms pause regardless of memory size. We deferred that to
v0.4+ because (a) the source-divergence sync mechanism hadn't been
sketched concretely enough to commit weeks to maintaining a
Firecracker fork, and (b) the engineering wins above get most of the
perceived value at a fraction of the cost — phase 1's 143× confirms
that intuition. The deferral is tracked in
[issue #101](https://github.com/deeplethe/forkd/issues/101); the
scaffolding (design doc, `forkd-uffd` handshake crate,
`MemoryBackend::Userfault` enum) stays in the repo as honest record
+ revival starting point. We explicitly chose **not** to fork
Firecracker — phase 1's 143× cleared 85 % of the original target on
vanilla upstream, and the memfd value-add doesn't add sharing
capability we don't already have. The `firecracker-patch/` directory
that originally held a patch sketch was deleted; reasoning in
[`docs/design/userfaultfd.md`](./design/userfaultfd.md) §
"Why we won't fork Firecracker".

## Integration patterns

Four concrete ways the projects can coexist or combine.

### Pattern 1: Side-by-side

The simplest. Run boxlite for the workloads it's strongest at
(stateful single-session boxes, embedded library use, multi-language
SDK consumers) and run forkd alongside for fork-heavy workloads
(speculative agent exploration, fan-out from warmed parent).

The agent or orchestrator picks per-step which backend to use.
Zero engineering on either side; the cost is that the user has
to deploy both.

### Pattern 2: boxlite calls a co-located forkd-controller for fork operations

A boxlite user who wants fork could run a forkd-controller
alongside boxlite. boxlite's box lifecycle would gain a
`box.fork(n)` method that, internally, talks to
forkd-controller's REST API to do the snapshot + restore-many.

This works without changing forkd's core. It does require boxlite
to add an optional dependency on a running forkd-controller, which
contradicts the "no daemon" thesis. The honest framing is "boxlite
remains daemon-free for the normal case; if you want fork, you
opt into running forkd alongside".

This is the lowest-cost shape of an actual collaboration.

### Pattern 3: forkd ships a library mode that embeds into boxlite

A bigger commitment from forkd's side. Today forkd is a daemon
because Firecracker's API ergonomics work better with a long-lived
controller. In principle, a library-mode forkd could:

- Embed the controller logic as a Rust library, not a subprocess.
- Expose `forkd::branch(source_handle, n)` as a function call.
- Let boxlite (or any other host application) link the library.

The blocker is that Firecracker itself is a subprocess, not a
library. Replacing Firecracker with libkrun (which IS library-mode)
would solve the embedding problem but require porting the fork
primitive to libkrun. That porting cost is substantial; libkrun's
snapshot machinery is less developed than Firecracker's, and the
"spawn N children sharing memory CoW" pattern would need to be
built from scratch on the libkrun side.

This pattern is real engineering, on the order of months, and
would change forkd's deployment story. We'd consider it if there
were a concrete user (boxlite or otherwise) committed to the
collaboration.

### Pattern 4: E2B SDK as the lingua franca

Both forkd's Python SDK and boxlite's library expose surfaces
similar to E2B's. If your agent uses the E2B SDK, you can swap
backends with an environment variable. The fork primitive is
unique to forkd; if your agent doesn't need it, boxlite is a
solid alternative.

This pattern requires no engineering on either side. It already
works.

## What we'd like to discuss

If we end up in a real conversation, things we'd be interested in:

1. **Reciprocal docs reference.** boxlite's docs mention forkd as
   the project to look at if you need fork-on-write; forkd's docs
   mention boxlite for the library/multi-lang/embedded use case.
   Each side keeps its independent positioning. Costs neither
   side anything.

2. **Joint technical content.** A blog post or whitepaper on
   "fork-on-write for AI agent compute" co-authored by both teams.
   Neutral hosting. Each side keeps its independent positioning.

3. **Cross-project benchmark.** Pick 3-5 representative agent
   workloads. Run on both projects. Publish raw data with no
   commentary. Let readers interpret.

4. **Pattern 2 as a proof-of-concept.** A worked example showing
   boxlite's `box.fork(n)` delegating to a co-located
   forkd-controller. Small enough to ship in a few days; concrete
   enough to validate the idea.

5. **Longer-term: library-mode forkd (Pattern 3).** Real
   engineering. Months of work. Worth discussing only if there's
   a specific user committed to the collaboration.

## A note on framing

forkd is the smaller project by stars and team size. boxlite has
distribution (multi-language SDKs, polished installer, 2k+ stars
in five months) we don't. We're not trying to court boxlite into
adopting forkd; we're trying to figure out whether the two
projects share enough that some flavor of collaboration helps
both audiences.

If the answer turns out to be no, that's fine. The COMPARISON
page ([`docs/COMPARISON.md`](./COMPARISON.md)) frames the four
projects in this space (Modal, CubeSandbox, boxlite, forkd) as
making different trade-offs in the same space, not as competing
for the same users. We believe that.

## See also

- [docs/COMPARISON.md](./COMPARISON.md), the public 4-way landscape note.
- [bench/pause-window/RESULTS-v0.2.md](../bench/pause-window/RESULTS-v0.2.md), the pause-window measurement (163 ms tmpfs, 4.26 s SSD).
- [bench/BOXLITE.md](../bench/BOXLITE.md), our N=100 spawn measurement against boxlite (113 s for boxlite, 1.06 s for forkd; the gap reflects different optimization targets).
- [boxlite README](https://github.com/boxlite-ai/boxlite), official project page.
- [docs/INTEGRATION-CUBESANDBOX.md](./INTEGRATION-CUBESANDBOX.md), the equivalent note for Tencent's CubeSandbox.
