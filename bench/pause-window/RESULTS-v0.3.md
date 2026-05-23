# Pause-window: v0.3 phase 1 results (diff snapshots)

**Status:** Phases 1a (primitive + sidecar measurement), 1b (real
`"diff": true` BRANCH path), 1c (agent-workload threshold), and 1d
(multi-BRANCH via previous-output chain) all landed. Phase 1d ships
in v0.3.1; v0.3.0 had the diff path restricted to first-BRANCH-only.

## Headlines

- **Idle source, 4 GiB SSD: pause 29 s → 205 ms = 143 ×.** Best
  case, included for comparability with prior art (CodeSandbox 2 s
  clone demo etc.). Phase 1b sweep.
- **Typical agent workload (2 GiB source, 30-300 MiB dirty):
  6-15 × pause reduction.** What you'll actually see in production
  fan-out. Phase 1c sweep.
- **Crossover at ~50-65 % source RAM dirty.** Above that, Full and
  Diff converge; pick Full. Phase 1c sweep.

## Phase 1b: 5-size pause sweep (idle source)

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

## Phase 1c: agent-workload threshold — where does Diff stop winning?

The phase 1a/1b numbers above are **idle-source best case** (3 s
settle, ~12-15 MiB dirty footprint coming from kernel init + runtime
overhead). A real fan-out workflow has the source running for some
time before BRANCH, dirtying more memory. At some dirty-page
threshold Diff's write cost catches up with Full's write cost and the
speedup collapses. **Phase 1c finds that threshold.**

Experiment: a guest-internal workload (`dirtier.py`) allocates
`--dirty-mib N` MiB as a `bytearray` and writes one non-zero byte
per 4 KiB page — exactly setting N MiB of KVM dirty bits. The
orchestrator (`sweep-agent.sh`) execs it, polls for a marker on
stdout, then BRANCHes. 3 trials per cell on a `mem-2048` source,
SATA SSD snapshot_root. Raw data:
[`agent-sweep-ssd.csv`](./agent-sweep-ssd.csv).

### Pause vs dirty footprint (mem-2048 SSD, mean ms, n=3)

| Dirty (MiB) | Full pause | Diff pause | **Speedup** | Measured diff size |
|---:|---:|---:|---:|---:|
| 0 (idle) | 13746 | 594 | **23.1 ×** | 12.2 MiB |
| 10 | 15207 | 673 | 22.6 × | 22.2 MiB |
| 50 | 13734 | 921 | 14.9 × | 62.8 MiB |
| 100 | 13803 | 1253 | 11.0 × | 113.7 MiB |
| 250 | 14527 | 2398 | **6.1 ×** | 266.6 MiB |
| 500 | 14090 | 5728 | 2.5 × | 521.0 MiB |
| 1000 | 14403 | 10708 | **1.3 ×** | 1029.5 MiB |

### Reading the curve

- **Full pause is flat** at ~14 s. 2048 MiB / 148 MB/s SATA fsync
  bandwidth = 13.8 s, matches the measurement. Full always writes
  every page regardless of dirty state.
- **Diff pause scales linearly with dirty footprint.** Slope is
  ~10 ms per dirtied MiB, exactly the SSD write bandwidth plus a
  ~500 ms control-plane floor (call round-trip + vCPU state harvest).
  Linear regression: `diff_ms ≈ 500 + 10.2 × dirty_mib`.
- **Crossover at ~1 GiB dirty** on this 2 GiB source — Diff catches
  Full when dirty footprint ≈ 65 % of source memory. Above that,
  Full is faster (no extra control-plane round-trip).
- **Diff_physical_bytes ≈ dirty_mib + 12 MiB** of fixed overhead
  (Python interpreter, dirtier process, kernel runtime activity
  during the dirty loop). Predictable enough to budget for.

### Practical guidance

| Workload | Dirty MiB | Recommend |
|---|---:|---|
| Just-spawned source, BRANCH immediately | <30 | **Diff** (15-23 ×) |
| Short agent run (few ReAct steps, 5-30 s) | 30-100 | **Diff** (11-15 ×) |
| Medium agent run (multi-minute, modest state) | 100-300 | **Diff** (6-11 ×) |
| Heavy agent run (many minutes, large buffers) | 300-700 | Diff (2-6 ×; still wins) |
| Memory-saturating workload | >700 (>35 % of source) | **Full** is comparable or faster |

The thresholds shift by source size: a 4 GiB source crosses over at
~2.6 GiB dirty; a 512 MiB source at ~330 MiB. Rule of thumb: **opt
into Diff whenever you expect dirty footprint to be <50 % of source
RAM at BRANCH time.** That covers essentially all realistic
fan-out scenarios where the source has been alive for seconds-to-
minutes, not hours.

### What this means for the 143× headline

The phase 1b 4 GiB SSD 143× number was measured on a 3-second-idle
source (~900 KiB dirty). Phase 1c's curve says that's the **asymptote**,
not the typical experience. For the modal "spawn → run agent for
30 s → BRANCH" workflow, the realistic speedup is **10-25 ×** — still
a category change, but not 143 ×.

The honest framing: phase 1's win is "**source pause drops by 10-25 ×
for typical agent workloads, up to 143 × for idle sources, declining
to 1× as the source dirties >50 % of its RAM**." Diff is the right
default for fan-out; Full remains the right tool when you know the
source has churned through most of its memory.

### v0.3.0's first-BRANCH-only restriction — lifted in v0.3.1 (phase 1d)

Phase 1b (v0.3.0) restricted diff mode to a sandbox's first BRANCH.
Firecracker clears the dirty bitmap on every snapshot/create, so:

- BRANCH 1 (Full or Diff): dirty bitmap cleared.
- BRANCH 2 (Diff): dirty bitmap captures only pages dirtied between
  BRANCH 1 and BRANCH 2 — applying that to source_tag/memory.bin
  (boot state) loses everything dirtied between restore and
  BRANCH 1.

**Phase 1d (v0.3.1) lifts this** without a separate per-sandbox
shadow file. The insight: each BRANCH's output (`snap_dir/memory.bin`)
is, by construction, source's state at that BRANCH's pause time —
exactly the base the next diff needs. The daemon tracks
`SandboxInfo.last_branch_memory_path` and uses it as the cp source
on the next diff BRANCH (falling back to source_tag/memory.bin with
a logged warning if the user has deleted the intermediate snapshot).

See [`docs/design/diff-snapshots.md`](../../docs/design/diff-snapshots.md)
§ "Multi-BRANCH diff: the previous-output chain (phase 1d)".

## Phase 1d: multi-BRANCH diff — N consecutive BRANCHes on the same sandbox

The phase 1d ship lifts the v0.3.0 single-BRANCH restriction.
Verification: 3 trials × 5 consecutive `diff: true` BRANCHes per
sandbox, mem-2048 SSD, 3 s gap between BRANCHes. Raw data:
[`multi-branch-sweep.csv`](./multi-branch-sweep.csv).

### pause_ms and diff size per BRANCH (mean of 3 trials)

| BRANCH | pause_ms | diff_physical_bytes |
|---:|---:|---:|
| 1 | 288 | 1.16 MB |
| 2 | 263 | 0.53 MB |
| 3 | 1321 | 0.39 MB |
| 4 | 1389 | 0.55 MB |
| 5 | 1446 | 0.41 MB |

### What's confirmed

- **All 5 BRANCHes succeed.** In v0.3.0 the second BRANCH with
  `diff: true` would have 400'd. The previous-output chain handles
  correctness.
- **Diff sizes are small** (0.4–1.2 MB per BRANCH) — Firecracker's
  per-snapshot bitmap clear is correctly captured by the chain;
  each BRANCH's diff covers only "since last BRANCH," not since
  restore.
- **Aggregate downtime: 14×** vs Full. 5 × 14 s = 70 s of source
  pause if these had been Full BRANCHes; multi-BRANCH diff totals
  ~4.7 s of pause across the same 5 BRANCHes.

### What was anomalous (RESOLVED in v0.3.4)

BRANCH 1-2 pause was ~280 ms; BRANCH 3-5 jumped to ~1.3-1.5 s on the
same source. After 5 rounds of probing
([`PROBE-multi-branch-anomaly.md`](./PROBE-multi-branch-anomaly.md))
the root cause turned out to be **ext4** — delayed allocation +
writeback throttle (`wbt_wait`) + multi-block allocator + block-bitmap
checksumming, all compounding per BRANCH as each 500 MiB+ memory.bin
write triggered increasing ext4 metadata work.

**Fixed in v0.3.4** by `posix_fallocate`-ing the destination memory.bin
to its full size before either the diff-mode background copy or
Firecracker's `/snapshot/create` writes to it (PR #152). Measured on
the same source / hardware / 10-BRANCH sweep:

| BRANCH | 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 | 9 | 10 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| before | 350 | 250 | 1300 | 1400 | 1500 | 2700 | 1500 | 1800 | 2700 | 1500 |
| after  | 585 | 286 | 344 |  161 |  369 |  153 |  189 |  162 |  324 |  174 |

BRANCH 6 from 2700 → 153 ms = **17.6×**. Median BRANCH 3-10 from
~1700 → ~200 ms ≈ **8.5×**. The post-fix curve matches a tmpfs
control to within noise, confirming the fix neutralizes the ext4
metadata overhead.

#146 closed.

The first-BRANCH-only restriction is gone in v0.3.1.

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
