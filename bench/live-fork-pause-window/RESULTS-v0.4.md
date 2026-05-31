# v0.4 live BRANCH pause-window results

Headline: `mode="live"` collapses the source-VM pause window from
**202 ms p50 (Diff)** to **56 ms p50 (Live)** on a 1.5 GiB source —
**3.6× faster** at the median, and the gap widens on slow storage
because Live's pause is disk-independent while Diff's is not.
`wait=false` lets the caller return after ~69 ms while the background
memory copy runs to completion asynchronously.

Methodology, raw numbers, and honest caveats below.

## TL;DR

| mode         | pause p50 | pause p90 | pause max | RT p50    |
|--------------|----------:|----------:|----------:|----------:|
| live-sync    |  **56 ms**|     64 ms |     64 ms |  13 730 ms |
| live-async   |     54 ms |    241 ms |    258 ms | **69 ms** |
| diff         |    202 ms |    418 ms |    434 ms |  13 461 ms |
| full         |  13 550 ms |  14 268 ms |  14 314 ms |  13 559 ms |

Key ratios at p50:

- **live vs diff**: 202 / 56 = **3.6× faster pause window**
- **live vs full**: 13 550 / 56 = **242× faster pause window**
- **async RT vs sync RT**: 13 730 / 69 = **198× faster return** for
  callers that don't need the snapshot bytes immediately

> "Pause" is the source VM's downtime (the user-visible gap in TCP
> connections, kvmclock, etc.). "RT" is the full HTTP round-trip on
> `POST /v1/sandboxes/<id>/branch` — this is what your code waits on.

## Setup

| Item            | Value                                                              |
|-----------------|--------------------------------------------------------------------|
| Host CPU        | 12th Gen Intel Core i7-12700 (8P+4E)                               |
| Host RAM        | 30 GiB                                                             |
| Host kernel     | Linux 6.14.0-36-generic (Ubuntu)                                   |
| Snapshot disk   | `/dev/sda2` — **WDC WD10EZEX-75WN4A1, ROTA=1 (spinning HDD)**, ext4 |
| Firecracker     | Vendored `forkd-v0.4-mem-backend-shared-v1.12` (musl release)      |
| Controller      | `feat(doctor,uffd): Phase 7.4` (commit `a372e2a`)                  |
| Source snapshot | `python-numpy` (from Hub, sha256-verified)                         |
| Source RAM size | 1 610 612 736 bytes = **1 536 MiB**                                |
| Iterations      | 10 per mode, modes interleaved (live-sync, live-async, diff, full) |
| Source sandbox  | Spawned once with `live_fork: true`; all BRANCHes hit it           |

Modes interleave so disk warm-up, page-cache fill, and any
process-wide drift contaminate all four modes equally instead of
biasing the last batch.

## Raw data

[`bench-live-fork.csv`](./bench-live-fork.csv) — one row per BRANCH
iteration; columns: `mode, iteration, http_round_trip_ms, pause_ms,
memory_bin_bytes, poll_until_ready_ms`.

Reproduced via [`bench-live-fork.py`](./bench-live-fork.py):

```bash
sudo python3 bench-live-fork.py \
    --source-tag python-numpy \
    --iterations 10 \
    --modes live-sync,live-async,diff,full
```

## What pause_ms measures

`pause_ms` is the source VM's vCPU-pause window:

- **`mode: "full"`**: Pause → write full `memory.bin` to disk → resume.
  Wall-bound by sequential disk write. On this HDD: ~120 MB/s, so
  1.5 GiB ≈ 13 s. SSD would cut this to ~3 s; NVMe ~1.5 s. Not
  acceptable for a running agent.
- **`mode: "diff"`**: Pause → snapshot vmstate + dirty pages → resume.
  Still wall-bound on disk write because the diff is *inside* the
  pause window. Tail goes wide as the snapshot's dirty page count
  grows (p90 = 418 ms is the cost of any one BRANCH hitting more
  dirty pages than the others).
- **`mode: "live"`**: Pause → snapshot vmstate, arm UFFD_WP, resume.
  The memory copy happens *after* resume, in a controller-side
  background thread. pause_ms is bounded by the vmstate dump
  (~30-50 ms for 1.5 GiB at our vmstate sizes) plus UFFD_WP arming
  on the resident regions (~0.4-0.6 ms in Phase 6 E2E).

This is why **the live pause window is disk-independent**: an NVMe
host wouldn't see Live get any faster (it's CPU-bound on vmstate +
WP arming), but Diff would still scale with disk speed. On slower
storage, the Live/Diff ratio gets *wider*, not narrower.

## What the round-trip column measures

`http_round_trip_ms` is what your code's `await ctrl.branchSandbox(...)`
or `c.branch_sandbox(...)` returns in:

- **live-sync (`wait=true`)**: blocks for source pause AND the
  background memory copy. p50 = 13 730 ms ≈ HDD throughput limit
  (same as Diff and Full).
- **live-async (`wait=false`)**: returns as soon as the source
  resumes. p50 = **69 ms**. The background copy still runs (and is
  visible via the `status` field flipping from `"writing"` to
  `"ready"`), but the caller doesn't wait on it.
- **diff / full**: synchronous by definition; same RT as live-sync.

The `wait=false` path is the headline UX win for agents: a `pause_ms
~ 56 ms` source downtime *and* a ~70 ms HTTP return. The bench
records `poll_until_ready_ms` separately so you can see when the
async snapshot is actually consumable — it's the same 13-14 s wall
time as sync BRANCH, just out of the critical path.

## Caveats

1. **Single host, single source size.** 1.5 GiB Python+numpy on i7-12700
   + HDD. Numbers will move with source RAM size (Live's pause is
   ~CPU + vmstate-size bound; Diff/Full are ~disk-bound) and with
   disk medium. We'd expect Live's headline gap to narrow on NVMe
   (because Diff gets faster) but never invert — Live is always
   bounded by the synchronous parts of FC's pause/dump path.

2. **`live-async` p90 outlier.** Iteration #8 saw pause_ms=258 ms
   (vs p50=54). Root cause not yet investigated; suspects: ext4
   writeback pressure from the in-flight previous async BRANCH, or
   FC's vmstate serialization hitting an irregularity. Reproducing
   on a clean disk and a longer run is the right follow-up. Median
   and p90 (excluding this point) stay tight.

3. **`unprivileged_userfaultfd=0` requires root for the bench.** The
   bench script runs the controller under `sudo` because
   `vm.unprivileged_userfaultfd=0` is the default on this dev box.
   Production deployments should either set the sysctl or give the
   controller `CAP_SYS_PTRACE`. `forkd doctor` (Phase 7.4) probes
   both.

4. **Source guest must be quiet during the BRANCH.** We ran
   python-numpy in its default warmed state with no in-guest
   workload. A guest under heavy write pressure during a Live BRANCH
   will see UFFD_WP capture more dirty pages, growing the bg-copy
   wall time (but NOT pause_ms — the pause stays disk-independent).

5. **`mode: "live"` requires the vendored Firecracker fork.**
   `mem_backend.shared = true` is the one upstream gap; tracked as
   [`FIRECRACKER-UPSTREAM-PROPOSAL.md`](../../FIRECRACKER-UPSTREAM-PROPOSAL.md).
   Once it lands upstream, the vendor requirement goes away.

## Comparison vs v0.3.4 Diff

v0.3.4 closed the multi-BRANCH compounding anomaly via
`posix_fallocate`, putting Diff at a steady ~150-300 ms on this same
hardware (see [`bench/pause-window/RESULTS-v0.3.md`](../pause-window/RESULTS-v0.3.md)).
This bench's Diff p50 of 202 ms lines up cleanly with that. The
v0.4 Live win is **on top of** v0.3.4 Diff, not against the original
v0.3.0 baseline.

For comparison:

| Version | Mode | p50 pause on this hardware |
|---------|------|---------------------------:|
| v0.2.x  | Full | ~13 500 ms                 |
| v0.3.0  | Diff | ~1 500-2 700 ms (anomaly)  |
| v0.3.4  | Diff | ~200 ms                    |
| v0.4    | Live | **~56 ms**                 |
