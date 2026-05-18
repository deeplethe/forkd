# forkd roadmap

Living document. Issues that are tracked individually live in
[GitHub issues](https://github.com/deeplethe/forkd/issues); this file
gives the high-level shape across releases.

Current release: **v0.2** (sandbox branching shipped, see commits
#49-#52 and [`docs/design/branching.md`](./design/branching.md)).

## v0.3 candidates — picked

### Live (no-pause) branching via userfaultfd

The single biggest technical bet in the v0.3 cycle, and a possible
top-venue paper (HotInfra '26 / NSDI '27).

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

**Idea.** Register the source VM's guest memory with `userfaultfd`,
keep the VM running, and copy pages on first child fault rather than
upfront. Cold-start floor drops from ~150 ms (today, dominated by
memory.bin mmap + restore) to ~10–30 ms (vCPU restore + first-page
fault round-trips).

**Why this is research-grade, not engineering.** Prior art (MITOSIS
NSDI '23, FaaSnap ATC '22, Klotski OSDI '22, NFork EuroSys '24) all
do related work but each makes different trade-offs: RDMA-backed
copy, snapshot speedup, VM-CoW for serverless, fork for FaaS. forkd
in this mode would be the first **open-source, mmap-based,
agent-oriented** live-fork — and the measurement story (pause window
deltas across 7+ systems on a common bench) is paper-shaped on its
own.

**Sketch (4–6 weeks part-time)**

| Phase | Weeks | What |
|---|---|---|
| 1 | 1-2 | Understand userfaultfd boundary: UFFDIO_REGISTER, UFFDIO_COPY/COPY_ZEROPAGE, UFFD_FEATURE_WP. Add `MemoryBackend::Userfault` mode to `forkd-vmm`. |
| 2 | 2-3 | Wire `restore_many_with` to register a fault handler thread per child instead of `mmap`-ing the entire memory.bin upfront. |
| 3 | 3-4 | Benchmark: cold-start floor on memory.bin sizes 256 MiB / 2 GiB / 8 GiB (vLLM-class). Target ~10–30 ms vs ~150 ms today. |
| 4 | 4-5 | A/B test against `postgres-fixture` and `playwright-browser` recipes. Capture page-fault rate over time as agents diverge. |
| 5 | 5-6 | HotInfra '26 paper first draft. Target submission ~July/August 2026. |

**Out of scope for v0.3.** Cross-host live branching (needs RDMA or
similar), persistent fault-handler dump-and-replay, fault-driven
prefetch policies (those are v0.4+).

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
