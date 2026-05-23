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

## Follow-up: thread-level attribution (2026-05-21)

Original probe used strace `-c` on the whole process, which can't
distinguish between *user-space CPU* and *off-CPU blocked waiting*.
Re-probed with two extra tools:

- **bpftrace `profile:hz:199`** on the FC pid, sampling on-CPU stacks
  for 30 s while firing a deliberately slow BRANCH inside that window.
  Result: only ~18 samples landed during the ~1.6 s slow BRANCH out of
  ~320 expected → **FC was off-CPU ~94 % of the BRANCH window**.
- **`/proc/$pid/task/*/stack` polled at 30 ms intervals** across all
  threads. Histogram of the top kernel-frame across the slow window:

| Count | Top kernel frame | Thread role | Meaning |
|---:|---|---|---|
| 90 | `ep_poll` | main | HTTP API socket idle — expected |
| 88 | `[kvm]` (in `kvm_vcpu_halt`) | vCPU | guest paused — expected, this *is* the pause |
| 50 | `vhost_task_fn` | vhost-net | idle — expected |
| 17 | `futex_wait_queue` | (unknown FC thread) | **blocked on a userspace futex/mutex** |
| 3 | `submit_bio_wait` | snapshot writer | block-layer IO completion wait |
| 2 | `jbd2_log_wait_commit` | snapshot writer | ext4 journal commit wait |

Full vCPU kernel stack for the 88 samples:

```
kvm_vcpu_block ← kvm_vcpu_halt ← kvm_arch_vcpu_ioctl_run
  ← vcpu_run ← kvm_vcpu_ioctl ← __x64_sys_ioctl
```

Full futex stack for the 17 samples:

```
futex_wait_queue ← futex_wait ← __futex_wait
  ← do_futex ← __x64_sys_futex
```

Standard "thread asleep on a userspace futex"; the kernel can't
identify *which* futex from a static stack.

### Revised picture

The pause-time growth has at least three contributors, not just user-space CPU:

1. **Userspace futex contention** (17/250 in-kernel-sleep samples).
   Some FC thread waits on a mutex/futex; bpftrace can't identify the
   specific futex without more invasive instrumentation. Hypothesis:
   the snapshot worker contends with vhost / vCPU teardown threads
   that are holding a shared state lock; lock hold-time may grow with
   accumulated snapshot count.
2. **ext4 journal + block IO writeback** (5 in-kernel-sleep samples,
   only ~2 % of off-CPU time but present every BRANCH). Snapshot's
   memory.bin write goes through the page cache → flusher → jbd2
   journal commit. On SSD this is bounded but visible.
3. **User-space CPU on the FC snapshot worker thread** — the
   un-attributable remainder (~70 % of off-CPU thread-poll points
   returned an empty kernel stack, meaning the thread was in
   user-mode). bpftrace's frame-pointer-based stack unwind fails on
   FC's static-pie release build → can't symbolize. Needs either
   DWARF unwinding (perf, when we can install it) or a debug FC
   rebuild.

### Implications for #118 Phase 2/3 — refined

- **Phase 2 (`io_uring` writer)** still doesn't address the dominant
  causes here. The block IO path is only ~2 % of the BRANCH window;
  futex contention and user CPU dominate.
- **Phase 3 (1 s pre-emptive tick)** would compound the futex
  contention if the snapshot worker holds the contended lock during
  its background work. Need to identify the lock before designing
  Phase 3.
- **New candidate work** surfaced by this probe: a one-week effort to
  identify the futex (use bpftrace's `tracepoint:syscalls:sys_enter_futex`
  with `args.uaddr` capture, correlate with stack of waker) → if
  contention is fixable, may cut multi-BRANCH pause growth without
  touching the IO path at all.

## Follow-up: futex tracing (2026-05-21)

Acting on the previous follow-up's "futex contention is the most
operational signal", added one more probe: bpftrace on
`tracepoint:syscalls:sys_enter_futex` / `sys_exit_futex` with
`args->uaddr` and `args->op` captured. Per-futex wait time and call
count aggregated.

### Result (8 s window covering one 153 ms BRANCH)

```
@wait_ns[0x79fa2c8775c8, 128]:    3.9k ns
@wait_ns[0x79fa2c878c88, 129]:    4.8k ns
...
@wait_ns[0x79fa2c8752a0, 137]:   1.14 ms   ← 6 calls
@wait_ns[0x79fa2c887648, 137]: 152.35 ms   ← 3 calls
@wait_ns[0x79fa2c878c88, 137]: 152.49 ms   ← 2 calls
@wait_ns[0x79fa2c878d08, 137]: 152.56 ms   ← 2 calls
```

Three different futexes each accumulated **~152 ms of wait time**
during the BRANCH window. Op 137 is `FUTEX_WAIT_BITSET_PRIVATE`.

The wait time per futex (~152 ms) ≈ the entire pause window. So:
**3 separate threads each sat blocked on a different futex for the
entire BRANCH duration**, then woke when the snapshot worker
finished.

### Where the futex addresses live (cross-ref `/proc/$pid/maps`)

```
0x79fa2c843000-0x79fa2c876000  rw-p  firecracker (.data/.bss)
0x79fa2c876000-0x79fa2c879000  rw-p  [anon, immediately after .bss]
0x79fa2c879000-0x79fa2c87d000  rw-p  [anon]
...
0x79fa2c885000-0x79fa2c88f000  rw-p  [anon]
```

Three of the hot futex addresses (`0x79fa...8c88`, `8d08`, `7648`)
fall in **anonymous heap mappings adjacent to FC's writable
section** — consistent with Rust heap allocations holding the inner
`AtomicU32` of a `parking_lot::Mutex` / `std::sync::Condvar`.

The 4th (`0x79fa2c8752a0`, 6 calls × 1.14 ms total, lower amplitude)
falls in **FC binary's `.data`/`.bss`** — could be a `static`/
`OnceCell`-held mutex, possibly `KVM_FD` or similar global.

### Why this matters

The 3 hot futexes pattern looks like a producer-consumer:
1 snapshot-worker thread doing the actual write, with 3 other
threads (vCPU pause acknowledger? vhost reaper? event-loop?) all
sleeping on `WAIT_BITSET_PRIVATE` for the snapshot worker to signal
completion. If the snapshot worker's wall time grows with snapshot
count (it does), all 3 waiters' wait time grows in lockstep.

This means **the contention is not the cause of slowness** — it's
the *symptom*. Eliminating the contention wouldn't speed anything
up; the bottleneck is whatever the snapshot worker is doing
single-threaded.

### Implications for #118 — third revision

The original Phase 2/3 scope was wrong; the first probe corrected to
"user-space CPU"; this one corrects further to "user-space CPU in
the snapshot worker, with 3 idle waiters parked on its completion
futex".

Operational next steps to actually identify the snapshot-worker
loop:

1. **Build a debug-symbols Firecracker** (`cargo build --release --features dwarf-symbols` or equivalent) so `perf record` / `bpftrace ustack(perf)` can resolve user-space frames to Rust function names.
2. **`perf record -F 99 -p $FC_PID -g`** during a slow BRANCH window
   (now that the dev box's `linux-tools-6.14.0-36-generic` ships
   without `perf`, this requires building perf from kernel source
   OR using a kernel version that has it). Flame graph the worker
   thread's stack.
3. **Cross-check against Firecracker's
   `vmm::persist::create_snapshot`** source. The function is ~21 KB
   of compiled code; if there's a per-snapshot growing data
   structure (memory region list, device descriptor vec, dirty
   bitmap walk), it should jump out.

### Files

- `bench/pause-window/probe-futex-trace.sh` — the bpftrace script
  that produced the data above. Writes /tmp/futex-trace-*.txt plus
  /tmp/futex-trace-*.txt.maps for uaddr cross-reference.

## Follow-up: perf flamegraph with DWARF Firecracker (2026-05-23)

Acting on the previous "build FC with DWARF + perf record" next-step.

### Setup
- Cloned `firecracker-microvm/firecracker@v1.12.0`
- Patched `Cargo.toml` `[profile.release]`: `lto = false` + `debug = "full"`
- Built with `RUSTFLAGS="-C force-frame-pointers=yes" cargo build --release --bin firecracker`
- Result: 50 MB binary with `.debug_info`, `.debug_line`, `.eh_frame_hdr`, frame pointers (`+0x871 vmm::vstate::vm::ArchVm::snapshot_memory_to_file` resolves perfectly)
- Swapped into `/usr/local/bin/firecracker` (backup at `firecracker.release.bak`)
- Lowered `kernel.perf_event_paranoid` 4 → 1 and `kernel.kptr_restrict` 1 → 0
  (paranoid=4 was silently producing 0-sample perf.data files)

### perf capture
`perf record -F 99 -a -g --call-graph fp -- sleep 10` while the
warmed source did 2 slow BRANCHes back-to-back inside the window.

```
warmup BRANCH 1: pause_ms=547
warmup BRANCH 2-5: 225-787
warmup BRANCH 6: pause_ms=2151    ← in slow regime
profiled BRANCH #1: pause_ms=2279
profiled BRANCH #2: pause_ms=1530

Captured 2479 samples / 2.2 MB perf.data
```

### Result — third interpretation flip

```
Samples by process (top 10):
  1176  swapper           ← CPU idle (47 % of samples)
    97  plymouthd
    78  bash
    73  sshd
    72  tokio-rt-worker
    69  runc:[2:INIT]
    66  kswapd0
    55  avahi-daemon
    53  redis-server
    46  kworker/u80:1+f
     1  firecracker       ← !!!

firecracker on-CPU leaf function:
     1  vmm::vstate::vm::<impl vmm::arch::x86_64::vm::ArchVm>::snapshot_memory_to_file
```

**Only ONE sample landed in firecracker** during 3.8 s of pause window.
That sample was in `snapshot_memory_to_file+0x871` (called from
`vmm::persist::create_snapshot+0x2c5`), which is the actual memory.bin
write path. But ~10 ms of on-CPU time can't explain 3.8 s of pause.

CPUs were dominated by `swapper` (idle) and unrelated processes
(sshd / plymouth / redis / etc. running their own work on other
cores). The pause time isn't burned in **user-space CPU at all**.

### Synthesis of all four probes

| Pass | Hypothesis | Verdict |
|---|---|---|
| 1 [#128](https://github.com/deeplethe/forkd/pull/128) — strace -c | User-space CPU bottleneck | Too strong — syscall fraction is small but doesn't imply user CPU dominates |
| 2 [#140](https://github.com/deeplethe/forkd/pull/140) — bpftrace + /proc/stack | 94 % off-CPU + futex waiters present | Right shape, wrong fixable lever |
| 3 [#143](https://github.com/deeplethe/forkd/pull/143) — futex args.uaddr | Futexes are passive waiters, not contention | Eliminates futex as a fix target |
| 4 [this] — perf -a -g + DWARF FC | **Pause time is in-kernel sleep with FC parked; CPU is mostly idle** | Final picture |

### Where the time really goes

FC issues the snapshot syscall chain (`KVM_GET_DIRTY_LOG` ioctl(s)
plus the file writes inside `snapshot_memory_to_file`), then **blocks
in the kernel**. While blocked:

- The CPU FC was running on goes idle (samples land in `swapper`)
- Other unrelated processes get to run on other CPUs
- 3 sibling FC threads (the futex waiters from #143) sleep awaiting
  the snapshot worker's completion signal
- vCPU thread sleeps in `kvm_vcpu_halt` (paused VM, expected)

Why does this kernel sleep get **longer** on BRANCH 3+? The kernel
operation FC is waiting on grows in cost. Most likely candidates:

1. **`KVM_GET_DIRTY_LOG` walk grows with VM uptime.** KVM tracks
   dirty pages in a per-memslot bitmap. The bitmap clears on every
   `KVM_GET_DIRTY_LOG` call but the kernel-side walk could touch
   structures that grow as guest pages get accessed over time.
2. **page-cache → block-IO writeback contention.** Each BRANCH's
   `memory.bin` write goes through the page cache. The N+1th
   BRANCH's write may compete with N's still-being-flushed pages.
   Background flusher (`kswapd0` / `kworker/u80:1+f` both visible
   in samples) running concurrently is suggestive.
3. **VMA / mmap teardown.** Each diff BRANCH adds a memory mapping.
   After many BRANCHes, FC's process VMA list grows; some kernel
   paths walk it linearly.

### What flamegraph would have shown (and didn't)

We'd hoped the flamegraph would point at a hot Rust function. It
doesn't, because FC isn't burning CPU — it's blocked. **On-CPU**
flamegraphing is the wrong tool here. We need an **off-CPU**
flamegraph (Brendan Gregg style — sample stacks at every
`sched_switch` event with duration weighting).

### Follow-up: off-CPU probe (same day, 2026-05-23)

Built an off-CPU bpftrace probe and ran it during the slow regime.
Used `tracepoint:sched:sched_switch` + `sched:sched_wakeup` paired by
TID, capturing `kstack(perf, 16)` at sleep time.

**Result — only the vCPU stack appears.** Out of all FC threads
(`firecracker`, `fc_api`, `fc_vcpu 0`, `fc_vcpu 1`, `kvm-nx-lpage-re`),
the only one going through sched_switch was `fc_vcpu`:

```
@offcpu_us[
    __schedule ← schedule ← kvm_vcpu_block ← kvm_vcpu_halt
    ← vcpu_run ← kvm_arch_vcpu_ioctl_run ← kvm_vcpu_ioctl
    ← __x64_sys_ioctl ← x64_sys_call ← do_syscall_64
]: 19260812
@offcpu_count[<same>]: 32
```

19.26 s of off-CPU time across 32 sleep cycles in vCPU 0+1. **No
other thread shows up at all** — not `firecracker` (the main thread),
not `fc_api`. They never `sched_switch` during the 8-12 s window.

Confirmed with a minimal probe (`tracepoint:sched:sched_switch / pid
== $FC_PID / { @[comm] = count(); }`) over 8 s with 5 warmup BRANCHes:

```
@switches_per_thread[fc_vcpu 0]: 6
@switches_per_thread[fc_vcpu 1]: 20
```

Only vCPU threads switch off-CPU. **Main thread + fc_api stay on the
CPU the entire window** (no sched_switch event) — *but they also
don't show up in perf on-CPU samples* (we have 1 FC sample in 2479).

### The paradox

```
- main thread isn't off-CPU (no sched_switch off)
- main thread isn't on-CPU (no perf samples)
- pause_ms grows linearly with BRANCH count anyway
```

A thread can be in this state only if:

1. **In a long uninterruptible kernel syscall** that runs to
   completion without preemption AND is somehow undersampled by
   perf. On Alder Lake hybrid (i7-12700, this dev box has P-cores +
   E-cores), perf hw event sampling can be E-core-blind by default —
   `cpu_atom/cycles/P` events need to be explicitly requested. Our
   perf record used default events which biased to P-core.
2. **Or perf sampling has a TOCTOU / IPI delivery issue** at the
   specific kernel state FC is in. Less likely.

### Concrete next step (deferred)

Re-run perf with explicit hybrid sampling:

```bash
perf record -F 99 -a -g --call-graph fp \
    -e cpu_core/cycles/P -e cpu_atom/cycles/P -- sleep 10
```

And confirm/refute the P-core/E-core sampling theory by
`taskset --cpu-list 0,1 forkd-controller serve ...` (force daemon &
its FC children to P-cores 0,1) before re-profiling.

If sampling is still empty: try `funccount` style probe on
suspected kernel hot paths — `kprobe:kvm_vm_ioctl`,
`kprobe:filemap_write_and_wait_range`, `kprobe:__filemap_fdatawrite_range`
— to spot which kernel path the main thread is camping in.

The full off-CPU step is documented but not done. **Below is the
original sketch retained for reference.**

### Original next step (off-CPU pair probe)

Off-CPU flamegraph via bpftrace:

```bpftrace
kprobe:finish_task_switch
/ args->prev->pid == $FC_PID /
{
    @start[tid] = nsecs;
}

kprobe:try_to_wake_up
/ @start[args->p->pid] != 0 /
{
    @offcpu[ustack(perf), kstack] = sum(nsecs - @start[args->p->pid]);
    delete(@start[args->p->pid]);
}
```

This sums up "how long was FC's worker thread asleep, broken down by
the (user-stack, kernel-stack) pair when it went to sleep." The
hottest pair tells us exactly which kernel function FC is parked on,
and which userspace caller put it there.

Estimated effort: 30 min once we get the bpftrace tracepoint right.

## ROOT CAUSE FOUND (2026-05-23, round 5)

### Hybrid CPU sampling found ext4 in FC's stacks

Re-ran perf with explicit hybrid events (`-e cpu_core/cycles/P -e
cpu_atom/cycles/P`) at 199 Hz over a 20 s window with 4 slow
BRANCHes inside. Got 13 FC samples (vs 1 before — the previous
round was Alder Lake E-core blind).

Sample bucketing:

| Category | Samples | % |
|---|---:|---:|
| FC user-space snapshot code | 6 | 46 % |
| **ext4 write / writeback / block-alloc** | **6** | **46 %** |
| KVM | 1 | 8 % |

The 6 ext4 stacks pointed at the same handful of kernel paths:

```
io_schedule ← wbt_wait ← rq_qos_wait ← __rq_qos_throttle
  ← blk_mq_submit_bio ← submit_bio_noacct ← ext4_bio_write_folio
  ← mpage_submit_folio ← mpage_map_and_submit_buffers
  ← ext4_do_writepages ← ext4_writepages
  ← do_writepages ← __filemap_fdatawrite ← ksys_write
```

```
down_write ← ext4_da_map_blocks.constprop.0 ← ext4_da_get_block_prep
  ← ext4_block_write_begin ← ext4_da_write_begin
  ← generic_perform_write ← ext4_buffered_write_iter
  ← ext4_file_write_iter ← vfs_write ← ksys_write
```

```
crc32c_x86_3way ← ext4_block_bitmap_csum_set ← ext4_mb_mark_context
  ← ext4_mb_mark_diskspace_used ← ext4_mb_new_blocks
  ← ext4_ext_map_blocks ← ext4_map_create_blocks ← ext4_map_blocks
```

Reading the names: ext4 **delayed allocation** + **writeback
throttle** (`wbt_wait`) + **multi-block allocator** + **block
bitmap checksumming**. Every BRANCH writes a 500 MB+ memory.bin;
ext4's metadata overhead compounds per BRANCH.

### tmpfs control: anomaly vanishes

To confirm fs-layer is the cause, spawned a second daemon
(`/dev/shm/forkd-snapshots` for `--snapshot-root`) and ran the same
10-BRANCH sweep:

| Storage | BRANCH 1 | 2 | 3 | 4 | 5 | 6 | 7-10 |
|---|---:|---:|---:|---:|---:|---:|---:|
| ext4 SSD | ~350 | ~250 | **1300** | **1400** | **1500** | **2700** | 1.5-2.7 s |
| **tmpfs** | 728 (cold) | **196** | **138** | **114** | **168** | **138** | **111-259** |

**On tmpfs, pause_ms stays in the 110-260 ms band for all 10
BRANCHes. No slow regime.** Definitive.

### Synthesis — five probe passes

| Pass | PR | Hypothesis | Truth |
|---|---|---|---|
| 1 [#128](https://github.com/deeplethe/forkd/pull/128) | strace -c | user-space CPU bottleneck | wrong |
| 2 [#140](https://github.com/deeplethe/forkd/pull/140) | bpftrace + /proc/stack | 94 % off-CPU + futex waiters | side observation |
| 3 [#143](https://github.com/deeplethe/forkd/pull/143) | futex args.uaddr | passive waiters, not contention | confirmed |
| 4 [#150](https://github.com/deeplethe/forkd/pull/150) | perf -a (P-core only) | in-kernel sleep dominates | direction right, sampling blind |
| 5 [this] | perf hybrid + tmpfs | **ext4 writeback throttle + mballoc + bitmap CRC** | **ROOT CAUSE** |

### Fix path (concrete)

Original #118 Phase 2/3 scope can finally be re-evaluated against
real data. The bottleneck is the snapshot file write path through
ext4, not KVM, not user CPU, not futex contention.

Easiest workaround (no code change):

- `--snapshot-root /dev/shm/forkd-snapshots` — flat pause, no growth.
  Drawback: not persistent (tmpfs cleared on reboot), and 16 GiB ceiling.

Real fixes, in order of leverage:

1. **`fallocate` the memory.bin to its full size before FC writes
   to it.** ext4 then doesn't need to run mballoc on each write
   range — the extent map is set. Tiny forkd-side patch in the
   spawn path; doesn't need FC changes.
2. **`O_DIRECT` writes** — bypass page cache + writeback throttle
   entirely. Requires upstream Firecracker change to its snapshot
   writer (single PR upstream).
3. **`io_uring` async writes** — Phase 2's original case, now with
   real data behind it.
4. **memfd-backed memory** — eliminate the on-disk memory.bin
   write entirely for BRANCH (the chain head stays in shared
   anonymous memory). Bigger refactor — touches forkd-vmm's
   diff-chain logic. Long-tail v0.4 candidate.

**Recommended first step**: try (1) `fallocate` in the daemon's
`spawn_one_for_branch` path. ~30 lines of Rust. If pause stays flat
post-fix → ship as forkd v0.3.4 patch + close #146.

## Files

- `bench/pause-window/probe-multi-branch-strace.sh` — the original
  per-BRANCH strace summary. Cheap; runs in ~50 s for N=10.
- `bench/pause-window/probe-bpftrace-fc.sh` — bpftrace user-stack
  sampling at 199 Hz; established the 94 % off-CPU finding.
- `bench/pause-window/probe-syscall-poll.sh` — /proc/$pid/syscall
  poll loop (200 Hz); did not pinpoint a single syscall (consistent
  with the off-CPU finding).
- `bench/pause-window/probe-futex-trace.sh` — bpftrace futex aggregator
  ("Follow-up: futex tracing" above).
- `bench/pause-window/probe-perf-flamegraph.sh` — perf record -a -g
  + DWARF FC; flipped the picture to "in-kernel sleep dominates"
  (round 4).
- `bench/pause-window/probe-offcpu.sh` — bpftrace off-CPU pair on
  sched_switch / sched_wakeup. Only catches vCPU halt — main thread
  doesn't sched_switch off (its slow syscall is on-CPU in kernel
  mode, hence not capturable here).
- `bench/pause-window/probe-perf-hybrid.sh` — perf with explicit
  hybrid event sampling (`cpu_core/cycles/P,cpu_atom/cycles/P`).
  This is the probe that found the ext4 stacks.
- This document.
