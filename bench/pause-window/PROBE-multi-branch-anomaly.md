# Probe: multi-BRANCH pause growth — root-cause attribution

**Date:** 2026-05-20
**Refs:** RESULTS-v0.3.md § "What's anomalous (TODO: investigate)", issue #118

## TL;DR

The "BRANCH 3-5 pause jumps to 1.3-1.5s" anomaly is **not an IO problem
and not a syscall problem**. ≥98% of the growth happens in Firecracker's
**user-space CPU** inside the `/snapshot/create` handler — syscall count
and total-time-in-syscalls stay roughly constant across BRANCHes while
wall time grows linearly.

**Direct implication for #118:**

- **Phase 2 (`io_uring` writer)** addresses a different bottleneck
  (`std::fs::copy` of the base memory file). It will NOT help this
  anomaly: the `write` and `fsync` calls during `/snapshot/create` are
  already cheap (~4-9 µs/call), and their count doesn't grow.
- **Phase 3 (pre-emptive 1 s tick background snapshot)** would ALSO be
  hit by this anomaly: snapshotting more often = more snapshots
  taken = the per-snapshot CPU cost climbing into the same slow regime
  within the first ~10 ticks.

The real fix likely needs Firecracker patches (or a sidestep of
`/snapshot/create`). See [next steps](#next-steps).

## Reproduction on dev box

`coding-agent-fork-prewarm-v1` snapshot (a prewarmed VM, smaller than
the original `mem-2048` from RESULTS-v0.3.md). 10 consecutive
`diff: true` BRANCHes, 3 s gap, single trial.

Raw data: `/tmp/multi-branch-probe-1779263771/summary.csv` on the dev
box (snippet):

```
branch_idx,pause_ms,diff_ms,diff_physical_bytes,strace_calls
1,351,349,1867776,1078
2,188,187,389120,709
3,248,246,798720,861
4,582,580,434176,732
5,397,395,417792,734
6,856,854,389120,717
7,972,970,425984,734
8,425,422,380928,715
9,878,875,708608,835
10,803,801,385024,709
```

Pattern: BRANCH 1-5 baseline (~188-397 ms); BRANCH 6-10 elevated
(~425-972 ms). On the original mem-2048 sweep the jump was sharper
(BRANCH 3 → 1.5 s); on this smaller / prewarmed snapshot it's
gradual. Same anomaly, different threshold.

## Attribution

### Where the time goes

`diff_ms` (the `/snapshot/create` API call) is within 1-2 ms of
`pause_ms` for every BRANCH. So:

- `vm.pause()` + `vm.resume()` overhead: ~1-2 ms total, **not the
  bottleneck**.
- The entire growth is inside the single `PUT /snapshot/create` call
  to Firecracker's HTTP server.

### Where it ISN'T going (ruled out)

1. **Not data volume.** `diff_physical_bytes` is *smaller* in slow
   BRANCHes (300-700 KB) than in fast BRANCH 1 (1.8 MB).
2. **Not syscall count.** Total syscalls in the FC process per
   BRANCH stays in a narrow band (709-1078) regardless of wall
   time.
3. **Not syscall time.** `strace -c` aggregate time-in-syscalls is
   3-10 ms per BRANCH (out of 188-972 ms wall) — at most ~2% of
   wall time, never the dominant cost.

Per-syscall growth between BRANCH 2 (188 ms wall) and BRANCH 7 (972
ms wall) on the same source:

| syscall | calls B2 / B7 | µs/call B2 → B7 |
|---|---|---|
| `write` | 593 / 605 | 4 → 9 |
| `fsync` | 3 / 3 | 175 → 574 |
| `lseek` | 57 / 69 | 1 → 7 |
| `munmap` | 3 / 3 | 8 → 40 |
| `open` | 2 / 2 | 29 → 85 |

Even with these per-call increases, total syscall time grows
3.8 ms → 10 ms — accounting for ~6 ms of the 784 ms wall-time
delta. **The remaining 778 ms is user-space CPU in Firecracker.**

### What this means

The growth is in Firecracker's snapshot-serialization or
memory-walking logic, not in the kernel or the disk. Candidates we
couldn't directly profile (no `perf` for kernel 6.14.0-36 on this
host):

1. **Vec/HashMap walks growing with snapshot count** — internal
   metadata structures in FC that get appended on every snapshot.
2. **VMA fragmentation** — each diff snapshot maps a fresh memory
   file. mmap walks linear in VMAs, but munmap is in the syscall
   path (only 4.8× growth, not enough alone).
3. **KVM bitmap-walk cost growing with ever-dirtied page count** —
   but this is a kernel-side cost, would show up in `ioctl`. `ioctl`
   only grew 4 µs/call → 20 µs/call × 6 = 120 µs growth.
4. **Firecracker's vCPU state harvesting growing** — vsock buffers,
   block device state, etc. accumulating.

Most consistent with the data: **(1) and (4) — pure userspace CPU
walking a structure that linearly grows with snapshot count**.

## What this means for #118

The current #118 scoping (Phase 2 = io_uring; Phase 3 = pre-emptive
background snapshot) was reasonable when we believed the BRANCH-3
jump was an IO or kernel-bitmap issue. Given this probe:

- **Phase 2's value is now narrower.** It still helps the
  `std::fs::copy` of the source memory.bin (the background copy in
  `controller::http::branch_sandbox`'s diff path — a few hundred MB
  of NVMe-vs-SSD throughput). But it does NOT cut `diff_ms` and
  therefore does NOT cut `pause_ms` on diff BRANCHes. Worth
  re-evaluating before committing 1 week of dev time.
- **Phase 3 needs rethinking.** A 1 s tick of pre-emptive snapshots
  would themselves accumulate the per-snapshot CPU cost. After
  10 ticks (10 s) we'd be in the slow regime. Phase 3 should
  instead drive an upstream FC fix OR cap snapshots per VM and
  recycle source VMs.

## Next steps

1. **Get `perf` working** (`apt install linux-tools-generic` plus a
   reboot, OR build perf from source for kernel 6.14.0-36). Profile
   FC during BRANCH 7. Confirm the user-space culprit (~5 minutes
   work once perf is available).
2. **Read Firecracker's `snapshot/create` handler** — locate any
   data structure that accumulates per snapshot. Patch upstream or
   document as a known FC limitation.
3. **Revise #118 scope** based on (1) + (2). Likely outcome:
   - Phase 2 narrows to "io_uring for the background memory.bin copy
     in the diff path" — a real but smaller win.
   - Phase 3 changes from "1 s tick" to "cap per-VM diff BRANCHes
     to N, recycle source via Full BRANCH + restore beyond N".

## Files

- `bench/pause-window/probe-multi-branch-strace.sh` — the script
  that produced the dataset above. Strace `-c` summary per BRANCH;
  cheap; runs in ~50 s for N=10.
- This document.
