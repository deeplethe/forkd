# forkd roadmap

Living document. Issues that are tracked individually live in
[GitHub issues](https://github.com/deeplethe/forkd/issues); this file
gives the high-level shape across releases.

Current release: **v0.2** (sandbox branching shipped, see commits
#49-#52 and [`docs/design/branching.md`](./design/branching.md)).

## v0.3 candidates — picked

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
`MemoryBackend::Userfault` enum, `firecracker-patch/`) is preserved
as record.

| Phase | What | Expected win | ETA |
|---|---|---|---|
| 1 | **Diff snapshots.** Firecracker already supports `enable_diff_snapshots: true` + `track_dirty_pages`. Wire forkd's BRANCH path to take diff snapshots when a parent exists for the source, so repeated fan-out from the same source only writes pages dirtied since the last snapshot. | 5–10x on 2nd+ BRANCH from the same source. Typical agent fan-out (1 source, N children, fork after some work) hits this case. | 3–5 days |
| 2 | **NVMe + io_uring snapshot writer.** Document the storage-tier choice, ship a daemon flag that uses io_uring for the memory.bin write when available. NVMe + io_uring already approximates what's achievable without changing the snapshot algorithm. | SSD 10×+ (~400 ms for 513 MiB, vs. 4.26 s today on SATA). | 1 week incl. measurement |
| 3 | **Pre-emptive background snapshot.** Background thread writes source's dirty pages to a staging memory.bin on a tick (1 s default). At BRANCH, only flush what's dirty since the last tick. Source's pause window becomes O(tick) instead of O(source memory). | Pause window bounded by tick interval (~50 ms for 1 s tick on a typical workload) regardless of source size. | 1–2 weeks |
| 4 | **Measurement + RESULTS-v0.3.md.** A/B numbers for each phase plus the stacked combo. Reuses the v0.2 bench harness. | Documentation. | 3 days |

Phases 1 and 2 are independently shippable. Phase 3 builds on phase 1's dirty-tracking
plumbing. The combination should reduce typical-workflow pause-window from seconds to
tens of milliseconds without changing the trust story (still vanilla Firecracker).

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
