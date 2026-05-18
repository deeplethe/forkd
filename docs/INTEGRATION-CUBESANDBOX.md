# forkd × CubeSandbox

A thoughtful walk-through of how these two open-source projects relate
and where they can interoperate. Written for engineers from either
team or operators considering deploying both.

## TL;DR

| | forkd | [CubeSandbox](https://github.com/TencentCloud/CubeSandbox) |
|---|---|---|
| **Position** | Fork-on-write microVM **primitive** | Full sandbox **runtime** with cluster scheduling |
| **VMM** | Firecracker | RustVMM (rust-vmm crates, tightly trimmed) |
| **Hypervisor** | KVM | KVM |
| **API surface** | REST `/v1/sandboxes/:id/branch` + Python `Controller` + `forkd` CLI | E2B-compatible SDK + REST `/v2/sandboxes` + dashboard |
| **Cold start** | ~150 ms (read-many from snapshot) | < 60 ms (pool pre-provision + clone) |
| **Per-instance memory** | shared parent CoW; child marginal | < 5 MiB per instance (CoW + trimmed runtime) |
| **Fork-on-write** | ✅ `POST /branch` primitive | 🚧 roadmap: "Event-level snapshot rollback (coming soon)" |
| **Multi-node scheduling** | ❌ single-host today | ✅ cluster with master + nodes |
| **E2B compatibility** | Python SDK only | E2B drop-in replacement |
| **License** | Apache-2.0 | Apache-2.0 |

**The takeaway**: forkd and CubeSandbox are **complementary**. forkd is
sharply focused on the fork primitive; CubeSandbox is a full runtime
with the cluster-side concerns forkd doesn't address. Where their
roadmaps touch — the "rollback / fork-from-snapshot" capability
CubeSandbox lists as "coming soon" — forkd already ships a working
implementation. There's a real opportunity to share code or
co-publish a joint engineering story.

## Where they overlap

Both projects make the same foundational decisions:

1. **KVM, not container.** Hardware isolation is non-negotiable for
   running untrusted AI-generated code.
2. **CoW memory sharing.** Memory amplification (1 parent's RAM
   serving N children) is the only way per-instance overhead stays
   in the single-digit MiB range.
3. **Snapshot/restore as the unit of distribution.** "Ship a warmed
   parent" is much faster than "ship a Dockerfile that builds a
   warmed parent."
4. **AI agent workloads as the target user.** Both projects are
   explicit about this in their READMEs.

## Where they differ

### VMM choice

CubeSandbox went all-in on **rust-vmm crates** assembled into a
custom, aggressively trimmed VMM. This is how they get to
**< 5 MiB per instance** — Firecracker carries machinery they don't
need.

forkd uses **Firecracker** as a dependency. Larger memory footprint
per child (typically 20-50 MiB before guest-side optimization), but:

- Battle-tested in AWS Lambda + Fargate
- Stable API surface; we don't ship VMM source
- Existing recipes (postgres-fixture, langgraph-react, agent-workbench)
  port directly

If you're at the "thousands of concurrent sandboxes on a single node"
end of the spectrum, **CubeSandbox's footprint wins**. If you're at
the "branch a stateful agent and explore 10 paths in parallel" end,
**forkd's primitive is more direct**.

### Fork-on-write

This is the most interesting divergence:

- **forkd**: `POST /v1/sandboxes/:id/branch` is the public, supported
  primitive. Pauses the source VM, writes a snapshot, resumes the
  source, and `mmap`s the snapshot into N children. The
  [pause-window benchmark](../bench/pause-window/RESULTS-v0.2.md)
  measures the trade-off honestly: ~4 s pause on a 513 MiB source.
- **CubeSandbox**: pause/resume endpoints exist (`/sandboxes/:id/pause`,
  `/sandboxes/:id/resume`), but **fork-from-snapshot is listed as
  "coming soon"** in the README. As of writing the README mentions:
  > Event-level snapshot rollback (coming soon): High-frequency
  > snapshot rollback at millisecond granularity, enabling rapid
  > fork-based exploration environments from any saved state.

This is precisely what forkd implements today. If a CubeSandbox user
needs this *now*, they can run forkd alongside CubeSandbox for the
fork-heavy slice of their workload.

### Scheduling

forkd is single-host: one daemon = one machine. The [v0.3 roadmap](../docs/ROADMAP.md)
mentions multi-node scheduling as speculative; we won't get there
until cross-host snapshot diffing lands.

CubeSandbox ships with a CubeMaster / Cubelet / CubeNet architecture
that handles scheduling, networking, and node coordination out of
the box. If you need to scale across machines today, CubeSandbox is
where you go.

## Integration patterns

Three concrete ways the two can coexist.

### Pattern 1: Side-by-side deployment

The simplest. You run **both** daemons on different ports, route
traffic by use case:

```
                 ┌─────────────────────────┐
                 │  agent orchestrator     │
                 └────┬────────────────┬──┘
                      │                │
       fork-heavy ────┤                ├──── steady-state
       speculative    │                │     scale-out
       exploration    ▼                ▼
                 ┌──────────┐    ┌──────────────┐
                 │  forkd   │    │ CubeSandbox  │
                 │  :8889   │    │  :8088       │
                 └──────────┘    └──────────────┘
                  Firecracker     RustVMM
```

Each project owns the workload it's strongest at. The agent talks
to whichever daemon's API matches its current step.

### Pattern 2: forkd as a CubeSandbox `/branch` backend

When CubeSandbox ships the "Event-level snapshot rollback" feature,
it will need *some* implementation strategy. One option: have
CubeSandbox's `/branch` endpoint delegate to a co-located
forkd-controller for the actual snapshot + restore-many work.

This is a real implementation path — both projects are Apache 2.0,
both use KVM, both have stable REST surfaces. The bridge would be:

1. CubeSandbox's `/sandboxes/:id/branch` (proposed) calls into a
   small bridge layer
2. Bridge translates CubeSandbox's internal sandbox identity to a
   forkd sandbox handle (this requires CubeSandbox's pause/resume
   to be compatible with forkd's snapshot format, or a translation
   layer)
3. forkd-controller does the actual pause+snapshot+restore-many
4. CubeSandbox returns the new sandbox handles to the caller

The blocker today: CubeSandbox's RustVMM snapshot format and
forkd's Firecracker snapshot format aren't binary-compatible. A
real implementation would either (a) have both daemons running
their own VMs in parallel, or (b) write a snapshot format
converter — non-trivial but mechanically possible.

**This is where the most interesting collaboration lives.** If
the CubeSandbox team is interested in shipping fork-on-write
without re-implementing it from scratch, forkd has done a lot of
the engineering already.

### Pattern 3: E2B SDK as the lingua franca

Both projects ship E2B-compatible APIs:

- CubeSandbox: drop-in E2B replacement at the daemon level
- forkd: `forkd.Sandbox` Python class matches E2B's surface

If your agent uses the E2B SDK, you can switch backends with one
environment variable. forkd vs CubeSandbox becomes a runtime
configuration choice, not a code change. The fork primitive is
unique to forkd — if your agent doesn't need it, CubeSandbox is a
fine alternative.

## What we'd love to talk about

If you're on the CubeSandbox team and reading this, we'd be
interested in:

- A joint technical blog post on the fork-on-write design space
- A worked example of pattern 1 or 2 above
- Cross-pollination of recipes — your sandbox templates have
  useful properties we'd like to learn from
- An honest comparison benchmark, hosted neutrally

[deeplethe](https://github.com/deeplethe) ships forkd; PR #236 on
your repo (storage cmdTimeout config) is a small example of the
direction we'd be excited to continue.

## See also

- [forkd ROADMAP.md](../docs/ROADMAP.md) — v0.3 userfaultfd plan
- [forkd pause-window benchmark](../bench/pause-window/RESULTS-v0.2.md) — the pause cost we measure today
- [CubeSandbox README](https://github.com/TencentCloud/CubeSandbox)
- [CubeSandbox OpenAPI spec](https://github.com/TencentCloud/CubeSandbox/blob/master/openapi.yml)
- [E2B SDK](https://e2b.dev) — the lingua franca both projects speak
