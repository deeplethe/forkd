# Pause-window: v0.3 phase 1a results (diff snapshots, idle-source A/B)

**Status:** Phase 1a measurement complete. Phase 1b (per-sandbox shadow
file so the diff path produces a restorable BRANCH, not just a
measurement artifact) is the next commit.

forkd v0.2 BRANCHes a running source by pausing it, writing the full
`memory.bin` to disk, and resuming. The pause is bandwidth-bound on
the snapshot-write step: 4.26 s ± 0.41 s on SATA SSD for a 513 MiB
source, scaling linearly with source RAM
([`RESULTS-v0.2.md`](./RESULTS-v0.2.md)).

v0.3 phase 1 swaps that for Firecracker's **Diff snapshot** mode,
which writes only the pages dirtied since the previous snapshot (or
since restore). For sources that haven't touched much memory between
BRANCHes — the typical fan-out case — Diff writes orders of magnitude
less data than Full, and the pause-window stops scaling with source
memory size.

## TL;DR

For an **idle source** (3 s settle between restore and BRANCH, no
guest workload), Diff snapshot pause-window is **roughly constant at
~250 ms regardless of source memory size**, because the source has
dirtied only ~900 KB of internal kernel state in 3 s. Full pause
scales linearly with memory (storage bandwidth × bytes). Speedup is
the ratio:

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

## Phase 1b: what comes next

The measurement above uses the daemon's `measure_diff` hook: BRANCH
takes a Diff snapshot for timing, then takes the Full snapshot
that's actually used as the BRANCH output. Both Full and Diff happen
inside the pause window, so the wall-clock pause is `Full +
Diff_throwaway`. The numbers above are `Diff` alone (subset of
`pause_ms`), not the actual cost the user experiences today.

Phase 1b replaces this with a real diff-based BRANCH path:
per-sandbox shadow `memory.bin`, Diff snapshot during pause,
apply-diff onto the shadow after resume (in background), children
mmap the shadow. Pause-window becomes the `Diff` column above; the
shadow-update happens off the critical path.

See [`docs/design/diff-snapshots.md`](../../docs/design/diff-snapshots.md)
for the full design, including the rejected alternatives and the
revival criteria for the v0.4+ live-fork path (issue
[#101](https://github.com/deeplethe/forkd/issues/101)).

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
- Sweep script: [`sweep-diff.sh`](./sweep-diff.sh). It reads
  `pause_ms`, `diff_ms`, `diff_physical_bytes`, `diff_logical_bytes`
  from the daemon's BRANCH response.

## See also

- [`RESULTS-v0.2.md`](./RESULTS-v0.2.md) — v0.2 baseline + prewarm fix.
- [`docs/design/diff-snapshots.md`](../../docs/design/diff-snapshots.md)
  — the phase 1 design.
- [`ROADMAP.md`](../../docs/ROADMAP.md) § "Cut pause-window without
  forking Firecracker" — the v0.3 plan this measurement is the first
  data point of.
