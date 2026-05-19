# forkd roadmap

Living document. Issues that are tracked individually live in
[GitHub issues](https://github.com/deeplethe/forkd/issues); this file
gives the high-level shape across releases.

Current release: **v0.3.1** ([release notes](https://github.com/deeplethe/forkd/releases/tag/v0.3.1)).
Source-pause window for `POST /v1/sandboxes/:id/branch` shipped at
**143× ceiling / 6-15× typical agent workload / 14× multi-BRANCH
aggregate** — see
[`bench/pause-window/RESULTS-v0.3.md`](../bench/pause-window/RESULTS-v0.3.md).

## What we're working on next — v0.3.x → v0.4 (6-item plan)

Post-v0.3.1 reprioritization (2026-05-19). The phase 2 / phase 3
pause-window items in the v0.3-candidates section below are real
but no longer top of the queue — current pause numbers are
already category-changing, and the bigger lever for adoption is
**distribution + ecosystem**, not further engineering on a number
that's already 6-15× ahead of vanilla. Sequence:

| # | Item | Effort | Cumulative | Track | Tracking |
|---|---|---|---|---|---|
| 1 | **MCP server** — `sdk/mcp/` finished, registered with Claude Desktop / Cursor / other MCP clients. forkd's BRANCH primitive becomes a tool any MCP agent can call. | 3-5 days | week 1 | distribution | TBD |
| 2 | **GitHub Action** — `uses: deeplethe/forkd-action@v1`. Open N sandboxes from a snapshot tag in CI; parallelize PR test runs / agent fan-out. Marketplace visibility. | 1 week | week 2 | distribution | TBD |
| 3 | **TypeScript SDK** — surface parity with the Python SDK (`Controller`, `Sandbox`, branch options). JS/TS is the larger half of the agent ecosystem. | 1-2 weeks | weeks 3-4 | distribution | TBD |
| 4 | **Stateful workspaces** — sessions that survive daemon / host restart, picking up where they left off. Daytona's killer feature; forkd's Hub + snapshot machinery is most of what's needed, the gap is UX. | 2 weeks | weeks 5-6 | stickiness | TBD |
| 5 | **3-5 agent-framework recipes** — `langgraph-react` exists; add `crewai-fanout`, `autogen-branch`, `openai-swarm`, `mcp-agent`. Each is a 2-3 day landing-page-grade demo. | 2-3 weeks | weeks 7-9 | adoption | TBD |
| 6 | **Phase 2 + 3 pause-window** — NVMe + io_uring snapshot writer + pre-emptive background snapshot. Targets total BRANCH API latency (today still bandwidth-bound on cp) and non-first-BRANCH pause. | 3 weeks | weeks 10-12 | technical depth | TBD |

**Track meaning**:
- *distribution* = make forkd reachable from where developers already are
- *stickiness* = make users come back / commit
- *adoption* = landing-page-grade material that converts the curious
- *technical depth* = keep the primitive ahead, but **only once we have users feeding us workload signal**

The reprioritization rationale: pause-window engineering past 143×
ceiling becomes a diminishing-returns problem until we have real
fan-out workloads pointing at where the next bottleneck actually
is. Items 1-3 unblock that user signal; item 6 returns to the
metric once we know what to optimize for.

## v0.3 candidates — picked (pause-window track)

### Cut pause-window without forking Firecracker

**Problem.** Today's `POST /v1/sandboxes/:id/branch` pauses the source
sandbox while `vm.snapshot_to()` writes `memory.bin` — typically
0.5–8 s depending on memory size. That window blocks the source's
TCP keepalives and progress; it's also the only remaining trade-off
in the branching primitive (see `docs/design/branching.md`).

**First-cut measurement (forkd v0.2).** Pause window is dominated
by snapshot-write throughput, not by VMM control-path work.
For a 513 MiB source running a TCP ping/pong agent:
**163 ms ± 7 ms on tmpfs-backed snapshot storage** (4 trials),
degrading to **4.26 s ± 0.41 s on SATA SSD with fsync** (5
trials). Same forkd code, only the storage backend differs.
External observers see the full gap; in-guest agents are nearly
pause-blind (connection survival 5/5, in-flight loss 0/5,
post-resume RTT returns to baseline) because kvmclock's
monotonic catch-up on resume races the recv data delivery. Full
methodology and raw data in
[`bench/pause-window/RESULTS-v0.2.md`](../bench/pause-window/RESULTS-v0.2.md).

**Approach.** Three engineering wins that stack and don't require any
Firecracker fork. The original "live branching via memfd + uffd_wp"
plan is deferred to v0.4+ — see
[issue #101](https://github.com/deeplethe/forkd/issues/101) for the
honest cost-benefit reasoning that led to the deferral. Scaffolding
from that earlier plan (the design doc, `crates/forkd-uffd/`,
`MemoryBackend::Userfault` enum) is preserved as record. The
`firecracker-patch/` directory was REMOVED in v0.3.0 after deciding
not to fork Firecracker — see
[`docs/design/userfaultfd.md`](./design/userfaultfd.md) §
"Why we won't fork Firecracker".

| Phase | What | Measured / expected | Status |
|---|---|---|---|
| 1 | **Diff snapshots.** `POST /v1/sandboxes/:id/branch` with `"diff": true`. Parallel cp + Diff during pause + apply on resume. Phase 1d (v0.3.1) added multi-BRANCH via previous-output chain — no separate shadow file. | **Source pause 29.3 s → 205 ms (143×) on 4 GiB SSD; 1.19 s → 190 ms (6.3×) on tmpfs. Typical agent workload (30-300 MiB dirty): 6-15×. 5 consecutive BRANCHes: 14× aggregate.** | **Shipped (PR #102, #107).** See [`docs/design/diff-snapshots.md`](./design/diff-snapshots.md) + [`bench/pause-window/RESULTS-v0.3.md`](../bench/pause-window/RESULTS-v0.3.md). |
| 2 | **NVMe + io_uring snapshot writer.** Daemon flag for memory.bin writes. Targets the underlying full-copy path (still bottlenecks total BRANCH API latency on SSD even with diff mode). | Expected SSD 10×+ on the full-copy backbone. | Pending. |
| 3 | **Pre-emptive background snapshot.** Background thread flushes dirty pages on a 1 s tick; at BRANCH, only flush what's dirty since the last tick. Bounds pause-window regardless of source size, including non-first BRANCHes. | Expected pause ≈ tick interval. | Pending. |
| 4 | **Measurement + RESULTS-v0.3.md.** A/B numbers for each phase plus the stacked combo. | Phase 1 measured (60 trials, 5 sizes × 2 backends × 2 modes × 3 trials). Phase 2/3 measurement pending. | Phase 1 done. |

Phase 1 covered forkd's killer use case (live BRANCH from a long-running source, where
source downtime matters more than total API latency). Phases 2 and 3 close the gap for
the remaining workloads (multi-BRANCH source, total-API-latency-sensitive callers).
The combination keeps the trust story unchanged (still vanilla Firecracker).

**Out of scope for v0.3.** Live-fork via memfd + uffd_wp (deferred, see
[#101](https://github.com/deeplethe/forkd/issues/101)). Cross-host live branching
(needs RDMA or similar). Persistent fault-handler dump-and-replay. Fault-driven
prefetch policies. These are v0.4+ candidates.

## v0.3 candidates — speculative

These don't have firm ship dates; revisit at v0.2.x retro.

- **Cross-host snapshot diffing** — ship a parent update as a binary
  diff against the previous tag instead of a full memory.bin. Big
  win for ~10 GiB ML-weight parents.
- **Branch GC policies** — auto-prune by age / count. Today every
  `branch_sandbox` call persists forever.
- **Merge-back / commit semantics** — `forkd merge --from <branch> --into <source>`
  to write a branch's diverged state back into the source's
  filesystem. Pairs with the "speculative destructive op" use case.
- **Multi-node scheduling** — break the "one daemon = one host"
  model. Probably depends on cross-host snapshot diffing landing
  first.
- **K8s Operator + CRDs** (`kind: ForkdSandbox`) — for downstream
  users with mature K8s platforms. Current `packaging/k8s/` starter
  manifest is the bridging step.

## Production-readiness gaps (across releases)

These are tracked separately from the v0.x feature line — they need
to land before v1.0:

- **Default-deny egress** on per-child netns. Today: shared
  MASQUERADE rule; allow-list policy = caller's responsibility.
- **`cpu.max` / `io.max` / `pids.max`** quotas beyond the existing
  `memory.max`.
- **Third-party security audit.**
- **Stable on-disk formats** — `snapshot.json` schema, `state.json`
  schema, audit log format all need a "v1.0 frozen" stamp.

## Recent shipped (v0.2 highlights)

- Sandbox branching: REST + CLI + Python SDK + volume inheritance +
  netns-allocator (#49 #50 #51 #52)
- forkd-mcp 0.1.0 on PyPI — MCP server for Claude Desktop / Code /
  Cursor / Cline
- K8s starter manifest verified end-to-end on k3s
- `postgres-fixture` recipe end-to-end verified
- 7-system bench refresh (CubeSandbox slow path → fast path,
  1.06 s / N=100 / 100 % success)
- Filed two upstream PRs to TencentCloud/CubeSandbox; #236 merged
