# Pause-window: v0.3 phase 1 results (diff snapshots)

**Status:** Phase 1a (primitive + measurement) and phase 1b (real
`"diff": true` BRANCH mode) both landed. Restriction: phase 1b's
diff mode is only valid for the first BRANCH per sandbox — see
"First-BRANCH-only restriction" below. Phase 1d (per-sandbox shadow
file to lift the restriction) is deferred to v0.3.1+.

## Headline: 4 GiB SSD source-pause **29 s → 205 ms = 143 ×**

The phase 1b real-mode A/B (5 memory sizes × 3 trials × 2 modes ×
2 backends = 60 trials):

| Source memory | SSD Full | **SSD Diff** | SSD speedup | tmpfs Full | tmpfs Diff | tmpfs speedup |
|---:|---:|---:|---:|---:|---:|---:|
| 256 MiB | 1807 ms | 241 ms | 7.5 × | 172 ms | 200 ms | 0.86 × |
| 512 MiB | 3414 ms | 226 ms | 15.1 × | 178 ms | 149 ms | 1.2 × |
| 1024 MiB | 6902 ms | 229 ms | 30.1 × | 324 ms | 194 ms | 1.7 × |
| 2048 MiB | 14508 ms | 222 ms | 65.4 × | 630 ms | 199 ms | 3.2 × |
| 4096 MiB | **29322 ms** | **205 ms** | **143 ×** | 1190 ms | 190 ms | 6.3 × |

Source pause-window is now essentially **constant at ~200 ms regardless
of source memory size**, because Diff's only cost is the
control-plane round-trip plus the small write of the dirty pages
(~900 KB for an idle source). Full pause scales linearly with memory
× storage bandwidth.

Caveats up front (details below):
- These are **idle-source** numbers (3 s settle). Real workloads with
  larger dirty footprints see proportionally smaller wins.
- Diff mode is **restricted to first BRANCH per sandbox** in v0.3.0
  (Firecracker's dirty bitmap is cleared on every snapshot). Multi-
  BRANCH support needs a per-sandbox shadow file, deferred.
- 256 MiB on tmpfs is a wash — diff's control-plane floor exceeds
  a fast-storage memcpy. Use Full for small-memory + fast-storage.
- Total BRANCH API latency is unchanged on SSD (the memory.bin copy
  still runs ~30 s in the background). Only **source downtime**
  shrinks. Right trade-off for live BRANCH from a running agent;
  wash for create-then-BRANCH-once.

## Phase 1a: the primitive in isolation

forkd v0.2 BRANCHes a running source by pausing it, writing the full
`memory.bin` to disk, and resuming. The pause is bandwidth-bound on
the snapshot-write step: 4.26 s ± 0.41 s on SATA SSD for a 513 MiB
source, scaling linearly with source RAM
([`RESULTS-v0.2.md`](./RESULTS-v0.2.md)).

v0.3 phase 1 swaps that for Firecracker's **Diff snapshot** mode,
which writes only the pages dirtied since the previous snapshot (or
since restore). Phase 1a took a Diff alongside the existing Full to
measure its cost in isolation — the numbers below predicted what
phase 1b's real diff-mode BRANCH would deliver. The phase 1b table
above is the actual user-visible cost; the phase 1a table here is the
underlying primitive cost.

Phase 1a numbers, idle source, 3 trials per cell:

| Source memory | SSD Full mean | SSD Diff mean | **SSD speedup** | tmpfs Full mean | tmpfs Diff mean | **tmpfs speedup** |
|---:|---:|---:|---:|---:|---:|---:|
| 256 MiB | 2198 ms | 267 ms | **8.2 ×** | 317 ms | 225 ms | 1.4 × |
| 512 MiB | 4053 ms | 233 ms | **17.4 ×** | 362 ms | 209 ms | 1.7 × |
| 1024 MiB | 7654 ms | 267 ms | **28.7 ×** | 539 ms | 236 ms | 2.3 × |
| 2048 MiB | 14993 ms | 242 ms | **62.0 ×** | 1097 ms | 223 ms | 4.9 × |
| 4096 MiB | 30414 ms | 239 ms | **127.3 ×** | 1394 ms | 268 ms | 5.2 × |

Raw data: [`diff-sweep-ssd.csv`](./diff-sweep-ssd.csv) and
[`diff-sweep-tmpfs.csv`](./diff-sweep-tmpfs.csv). 3 trials per cell;
SETTLE_SECS=3 between source spawn and BRANCH.

## What you're seeing

**Diff time is roughly constant** because the source is idle. The
dirty footprint reported in `diff_physical_bytes` is ~900 KiB across
all sizes — that's Linux kernel runtime overhead (init, timekeeping,
internal allocator activity) accumulating over 3 s. **The
diff-to-logical compression ratio drops from 0.34 % at 256 MiB to
0.02 % at 4 GiB**: the bigger the source, the smaller the fraction
of its memory the dirty bitmap covers.

**Full time scales linearly with memory** because writing the full
memory.bin is bandwidth-bound. The SSD column tracks 148 MB/s fsync
throughput (matches the `dd conv=fsync` floor measured in
`RESULTS-v0.2.md`). The tmpfs column tracks ~3 GB/s memcpy bandwidth.

**Diff floor is ~200-270 ms** even at 256 MiB — that's the
control-plane cost (PUT /snapshot/create round-trip, vCPU state
harvest, sparse file write of the tiny dirty pages). This floor
doesn't shrink with source memory.

## The caveat that matters

These numbers are the **best case**. Idle-source diffs are tiny, so
Diff timing approaches the control-plane floor. **Real fan-out
workloads — agents that have been running for 30 s and dirtied
maybe 100 MB of working set — will see proportionally smaller
speedups**, because the diff write itself becomes the bottleneck
again.

Back-of-envelope for 100 MB dirty footprint on SSD:
- Diff cost ≈ control-plane (~200 ms) + write 100 MB / 148 MB/s
  ≈ 200 + 676 = ~880 ms.
- Full cost (4 GiB source) ≈ 30 s.
- Speedup: ~34 ×.

Still a huge win for fan-out, but not the **127 ×** the idle bench
shows. Phase 1b's measurement will inject a real workload (an agent
allocating and touching a buffer between BRANCHes) and re-measure.

## When does Diff *not* help?

- **First BRANCH on a long-running source.** Firecracker's dirty
  bitmap starts populated at restore time — every page touched since
  the source booted from snapshot counts as dirty until the first
  snapshot clears it. A source that's been running for an hour can
  have a near-full dirty set on its first Diff, degrading to Full
  performance. Subsequent Diffs are fast (the bitmap was cleared).
- **Sources with high memory churn** (large workloads, ML inference
  with KV-cache turnover, browsers under heavy use). Dirty footprint
  per BRANCH approaches full memory, so Diff loses its advantage.
- **One-shot BRANCH** (create source, BRANCH once, discard). The
  Full path is one operation; Diff requires keeping a base around
  for the merge. Phase 1b's shadow-file machinery is amortized
  across multiple BRANCHes, not a one-shot win.

## Phase 1b: real diff-mode BRANCH (`"diff": true`)

The phase 1a numbers above used the `measure_diff` sidecar — they
measure how long a Diff snapshot WOULD take, while the user still
paid the Full pause. Phase 1b ships the actual diff-mode BRANCH:
`POST /v1/sandboxes/:id/branch` with `"diff": true` parallelizes the
source-tag memory.bin copy with the source running, takes a Diff
snapshot during pause, resumes the source, and merges the diff onto
the (already-copied) snapshot output. **The pause-window is the Diff
window — nothing else.**

15 trials per backend (5 sizes × 3 trials) per mode (Full vs Diff)
on fresh sources. Phase 1b restricts diff BRANCH to the first BRANCH
per sandbox (Firecracker clears the dirty bitmap on every
snapshot/create, so a second Diff would miss pages dirtied before
BRANCH 1 — see "First-BRANCH-only restriction" in the design doc).

### User-visible pause_ms — Full vs Diff (n=3 per cell)

| Source memory | SSD Full | SSD Diff | **SSD speedup** | tmpfs Full | tmpfs Diff | **tmpfs speedup** |
|---:|---:|---:|---:|---:|---:|---:|
| 256 MiB | 1807 ms | 241 ms | **7.5 ×** | 172 ms | 200 ms | 0.86 × |
| 512 MiB | 3414 ms | 226 ms | **15.1 ×** | 178 ms | 149 ms | 1.2 × |
| 1024 MiB | 6902 ms | 229 ms | **30.1 ×** | 324 ms | 194 ms | 1.7 × |
| 2048 MiB | 14508 ms | 222 ms | **65.4 ×** | 630 ms | 199 ms | 3.2 × |
| 4096 MiB | 29322 ms | 205 ms | **143 ×** | 1190 ms | 190 ms | **6.3 ×** |

Raw data: [`diff-real-sweep-ssd.csv`](./diff-real-sweep-ssd.csv) and
[`diff-real-sweep-tmpfs.csv`](./diff-real-sweep-tmpfs.csv). Sweep
script: [`sweep-diff-real.sh`](./sweep-diff-real.sh).

### What changed vs phase 1a

The phase 1a numbers were the THEORETICAL diff cost (the Diff sidecar
inside the still-Full pause window). Phase 1b's numbers are the
ACTUAL pause cost the user experiences with `"diff": true`. They
match phase 1a's projections within measurement noise:

- 4 GiB SSD phase 1a: 239 ms diff. Phase 1b: 205 ms pause. Match.
- 4 GiB tmpfs phase 1a: 268 ms diff. Phase 1b: 190 ms pause. Match.

The match confirms the architecture works: source pauses for the
diff window, then resumes; the cp + apply_diff happens off the
critical path.

### What 256 MiB tmpfs is telling us

The tmpfs 256 MiB cell shows diff (200 ms) being SLOWER than full
(172 ms). At small memory + fast storage, Firecracker's control-plane
floor for taking a Diff snapshot (~190 ms — call setup, sparse-file
allocation, vCPU state harvest) exceeds the cost of just memcpy'ing
256 MiB to tmpfs. **Diff is the wrong tool when source memory is
small AND the storage backend is fast.** Recommendation: leave the
default at Full; opt into Diff via the request body when source is
≥512 MiB and snapshot_root is on real disk.

### Where the time actually goes in diff mode

For 4 GiB SSD diff mode, the user sees `pause_ms = 205`. The
breakdown:

- Source pause window: 205 ms (this is `pause_ms`).
- Background memory.bin copy: ~30 s (runs in parallel with source).
- Post-resume apply_diff merge: ~10 ms (962 KB of diff data onto the
  pre-copied 4 GiB base).
- Total BRANCH wall-clock (sandbox-create returns to caller): ~30 s,
  bottlenecked by the copy.

**Source downtime drops 143 ×; total BRANCH API latency is unchanged.**
That's the right trade-off for forkd's killer use case (live BRANCH
from a long-running agent where TCP connections and timers matter)
and a wash for create-then-BRANCH-once-and-discard (where total time
is what matters).

### The first-BRANCH-only restriction

Phase 1b's diff mode is restricted to a sandbox's first BRANCH.
Firecracker clears the dirty bitmap on every snapshot/create, so:

- BRANCH 1 (Full or Diff): dirty bitmap cleared.
- BRANCH 2 (Diff): dirty bitmap captures only pages dirtied between
  BRANCH 1 and BRANCH 2 — applying that to source_tag/memory.bin
  (boot state) loses everything dirtied between restore and
  BRANCH 1.

The daemon enforces this with `SandboxInfo.has_branched: bool`. A
second `"diff": true` on a sandbox already BRANCHed gets a 400 with
a pointer to use Full mode instead.

Phase 1d (deferred to v0.3.1+) lifts this with a per-sandbox shadow
file. For v0.3.0 the restriction is acceptable because forkd's
canonical fan-out workflow ("spawn → BRANCH once → fan out N → discard
source") only ever takes one BRANCH per sandbox anyway.

See [`docs/design/diff-snapshots.md`](../../docs/design/diff-snapshots.md)
for the full design.

## Methodology notes

- 5 source memory sizes: 256 / 512 / 1024 / 2048 / 4096 MiB. Built
  via `forkd snapshot --mem-size-mib N --tag mem-N ...` from the
  `langgraph-react` rootfs (Python 3.12 + requests).
- Daemon spawned with `enable_diff_snapshots: true` baked into
  `forkd_vmm::ForkOpts` for daemon-path sources — required by
  Firecracker for the resulting VM to admit Diff `/snapshot/create`
  calls.
- 3 trials per (memory, backend) cell. SETTLE_SECS=3.
- SSD: `--snapshot-root ~/.local/share/forkd/snapshots` on an
  Ubuntu 24.04 host's root filesystem (148 MB/s fsync).
- tmpfs: `--snapshot-root /dev/shm/forkd-snapshots` after copying the
  5 source snapshots into `/dev/shm`.
- Phase 1a sweep script:
  [`sweep-diff.sh`](./sweep-diff.sh) — measure_diff sidecar on top
  of Full BRANCHes.
- Phase 1b sweep script:
  [`sweep-diff-real.sh`](./sweep-diff-real.sh) — `"diff": true` A/B
  against `"diff": false`. Each trial is a fresh source.

## See also

- [`RESULTS-v0.2.md`](./RESULTS-v0.2.md) — v0.2 baseline + prewarm fix.
- [`docs/design/diff-snapshots.md`](../../docs/design/diff-snapshots.md)
  — the phase 1 design.
- [`ROADMAP.md`](../../docs/ROADMAP.md) § "Cut pause-window without
  forking Firecracker" — the v0.3 plan this measurement is the first
  data point of.
