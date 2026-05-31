# v0.4: live-fork via userfaultfd write-protect

**Status:** IMPLEMENTED — the design described below is wired up end-to-end
on the user surface (Phases 6 + 7, May 2026). REST `mode: "live"`, CLI
`--live`, Python / TypeScript / MCP SDKs, and `forkd doctor` capability
checks all shipped via PRs
[#194](https://github.com/deeplethe/forkd/pull/194)–[#207](https://github.com/deeplethe/forkd/pull/207).
The vendored Firecracker dependency lives at
[deeplethe/firecracker:forkd-v0.4-mem-backend-shared-v1.12](https://github.com/deeplethe/firecracker/tree/forkd-v0.4-mem-backend-shared-v1.12);
upstream proposal is open
([`FIRECRACKER-UPSTREAM-PROPOSAL.md`](./FIRECRACKER-UPSTREAM-PROPOSAL.md)).
Clean-parent bench (`bench/live-fork-pause-window.md`) still pending —
Phase 6 E2E saw pause_ms = 41-48 ms, but on a parent with pre-baked
guest Oopses contaminating the measurement.

The original DRAFT below is preserved verbatim as the architecture
record; the implementation tracks it closely.
**Tracking issue:** [#101](https://github.com/deeplethe/forkd/issues/101)

## Motivation

v0.3.4 BRANCH (diff snapshot) takes ~150–300 ms on ext4 + SSD, of
which essentially all is a *hard pause window* — the source VM cannot
execute guest code while `memory.bin` is being written. For an agent
that does interactive inference, 150 ms straddles the perceptible-delay
boundary. For an agent that BRANCHes often (speculative-execution
patterns, live-rollout evaluation), it compounds: every branch point
freezes the parent.

The pause is structural in v0.3.4. The daemon issues Firecracker's
`Snapshot.Create`, which:

1. Pauses the source VM (microseconds).
2. Writes `vmstate` JSON (KB-scale, microseconds).
3. Writes `memory.bin` (500 MiB+ for a typical Python+JIT parent,
   tens of milliseconds even on tmpfs, hundreds of milliseconds on
   ext4 — see `bench/pause-window/PROBE-multi-branch-anomaly.md` for
   the v0.3.4 fix story).
4. Resumes the source VM.

Step 3 dominates. As long as `memory.bin` is written synchronously
inside the pause, we can only optimize within the disk-write cost.
v0.3.4 squeezed out the ext4 metadata penalty via `posix_fallocate`;
that's about as far as the synchronous path can go.

## Goal

Reduce the BRANCH pause window from ~150 ms to **< 10 ms** by removing
the synchronous memory write entirely. The vCPU + device state dump
still requires a pause (KVM_GET_REGS, KVM_GET_SREGS, virtio
descriptor snapshotting, kvmclock fixup), but that's a few KB of state
and tens of microseconds, not hundreds of milliseconds.

Stretch goal: pause < 1 ms.

## Non-goals

- Cross-host BRANCH (deferred to v0.5).
- Non-Linux backends (libkrun port is its own multi-month effort).
- Reducing child-spawn latency (already ~20 ms/child, not the
  bottleneck — children just `mmap(MAP_PRIVATE)` the snapshot).
- Lazy-restore on the child side (children already inherit memory via
  CoW, the cost is in BRANCH not in spawn).

## Proposed approach

Three building blocks:

### 1. `memfd_create` for source RAM

Replace the current file-backed guest memory mmap with anonymous memfd.
This is necessary because `UFFDIO_WRITEPROTECT` is supported on
anonymous and shmem-backed VMAs but not on arbitrary
host-filesystem-backed mmaps. memfd is technically tmpfs-backed and
qualifies. (Reference: kernel commit `1df319f0837c`, "userfaultfd: wp:
add WP support for shmem".)

Practically this is a swap of the backing in `forkd-vmm`'s memory
setup — the guest still sees a contiguous physical address space, the
host backing just changes from a file to a memfd.

### 2. `UFFDIO_WRITEPROTECT` on the source memfd before BRANCH

Register a `userfaultfd` against the source's memory region, then
issue `UFFDIO_WRITEPROTECT` over the full guest physical address space
in one syscall. The source VM continues running. Any subsequent guest
write to a still-WP'd page traps into the userspace handler before
the write commits.

The WP-arming cost is approximately O(VMA size / page-table walk
cost). On tested kernels (6.14, 5.7+) this is sub-millisecond for
multi-GiB regions when THPs are split appropriately.

### 3. Async dirty-page copier

A handler thread polls the uffd file descriptor. For each WP fault:

```
1. Read the page out of the source memfd at (faulting_addr - base).
2. Append the page (with its offset) to the in-flight snapshot file.
3. Clear the WP bit for that page (UFFDIO_WRITEPROTECT with mode=0).
4. Wake the faulting thread (UFFDIO_WAKE).
```

In parallel, a *bulk copier* reads still-clean pages from the source
memfd directly (no faulting involved, the memfd is just memory) and
writes them to the snapshot file. The two flows coordinate through a
per-page state map (clean / dirty-copying / final) so each page is
written exactly once.

The snapshot file is therefore complete some time *after* the BRANCH
pause exits, but it represents the consistent point-in-time view from
the moment WP was armed.

### What the pause window contains

After the changes above, the BRANCH critical section reduces to:

- vCPU dump: `KVM_GET_REGS` + `KVM_GET_SREGS` + a few model-specific
  registers, microseconds.
- Device state dump: virtio descriptor heads, MMIO state, microseconds.
- WP arming: `UFFDIO_WRITEPROTECT` over the whole RAM region, target
  sub-millisecond.
- kvmclock + TSC offset snapshot for guest time continuity, microseconds.

Total: well under 10 ms, and most of it independent of guest RAM size.

## Alternatives considered

### A) Status quo: pause-based snapshot

What we have today. Simple, robust, well-understood. Cost: ~150 ms
pause per BRANCH on ext4 + SSD. Becomes prohibitive when BRANCHing
>1/s, which is exactly the speculative-execution pattern this project
exists to enable.

### B) Pre-copy (à la live migration)

Iteratively dirty-track pages via `KVM_GET_DIRTY_LOG` and copy them in
rounds while the source keeps running, ending with a small "stop and
copy" final pass. This is the standard cross-host VM migration design
(Clark et al. NSDI 2005).

Downsides for our use case:

- `KVM_GET_DIRTY_LOG` requires `KVM_MEM_LOG_DIRTY_PAGES` to be set on
  memslots, which has its own per-`KVM_RUN` overhead.
- The "convergence" problem: if the guest's dirty rate exceeds copy
  bandwidth, pre-copy never finishes. Some agent workloads
  (`memset`-heavy initialization, large allocations during training)
  hit this regime.
- More implementation surface than uffd_wp.

### C) Full memcpy-out-then-snapshot

Pause briefly, `memcpy()` the entire guest RAM into a second buffer,
resume the guest, then async-write the buffer to disk. Pause cost:
memcpy time, roughly 5 ms/GiB on modern DDR. Memory cost: 2× peak
RAM usage.

The 2× RAM cost is a dealbreaker for the AI fan-out use case, where
parent VMs are routinely 4-8 GiB and the host already runs many of
them.

### D) Block-device CoW (LVM, dm-snapshot, btrfs reflink)

Snapshot the underlying block device, not the RAM. Doesn't apply:
guest RAM lives in memfd/file mappings, not on a block device. The
disk-backed virtio-blk *content* could be CoW'd this way, but that's
a separate problem from RAM snapshots.

uffd_wp is the right choice because it's the only mechanism that
gives us per-page lazy copy with no pause for clean pages and no
second memory buffer.

## Open questions

These are genuine unknowns. Reach out via issue if you have
experience here:

1. **Behavior of `UFFD_WP` on memfd-backed VMAs under `KVM_RUN`.** Are
   there any KVM paths that bypass userspace faulting and access
   guest memory directly (e.g., for MMIO emulation, virtio descriptor
   walking, kvmclock updates from the host side)? If so, do those
   paths get `UFFD_WP` write-faults, or do they silently violate the
   WP invariant? My current reading of `kvm_main.c` is that
   `gfn_to_hva_*` paths *do* go through the WP, but I haven't
   verified empirically.

2. **Interaction with transparent hugepages.** If the source memfd is
   backed by THPs, `UFFD_WP` works at the 4 KiB level — does the
   kernel split the hugepage on the first WP-fault, or does it WP the
   whole 2 MiB region? Splitting on each fault could be expensive
   for sparse-write workloads. May need to disable THP for source
   VMAs explicitly.

3. **vCPU dirty-bitmap vs uffd_wp.** KVM tracks its own dirty pages
   via `KVM_GET_DIRTY_LOG`. Is there value in combining both (e.g.,
   pre-write the KVM-dirty subset eagerly, then arm WP only on the
   clean remainder) or does uffd_wp on the whole region subsume it?
   The combined approach saves faults for the hottest pages but
   doubles the bookkeeping.

4. **Snapshot file format compatibility.** v0.3.4's snapshot is
   `vmstate JSON + memory.bin (contiguous raw 4 KiB pages)`. v0.4
   needs either (a) sparse memory.bin with page offsets, or
   (b) a chunked/segmented memory.bin format. Leaning (a) since
   stock Firecracker's restore expects contiguous; (b) breaks
   restore compatibility.

5. **Children spawned mid-BRANCH.** A child could in principle start
   `mmap`'ing the snapshot file before all dirty pages have been
   flushed, since the parent's pre-BRANCH state is consistent the
   moment WP is armed. Implementation requires the snapshot reader
   to block on in-flight pages with proper synchronization. Out of
   scope for v0.4 first cut, but a fast follow.

## Implementation phases

### Phase 1: standalone PoC (Week 1-2)

A separate Rust binary, not yet integrated with forkd. Allocates a
1 GiB memfd, populates with patterns, registers uffd, arms WP, forks
a writer process that randomly writes the memfd, captures faults,
copies dirty pages to a snapshot file, validates that the snapshot is
a consistent point-in-time view. Goal: prove the kernel mechanics work
as expected outside the KVM context.

### Phase 2: integrate into `forkd-uffd` crate (Week 3-4)

Extend the existing `crates/forkd-uffd/` (currently used for
restore-side lazy paging) with a snapshot-side WP path. Plumb the new
flow through `forkd-controller::branch_sandbox`. Add a `--live-fork`
feature flag (default off) so the v0.3.4 pause-based path remains
available during stabilization.

### Phase 3: pause-window benchmarking (Week 5)

Reproduce the v0.3.4 multi-BRANCH sweep
(`bench/pause-window/sweep-diff.sh`) but with `--live-fork`. Target:
pause < 10 ms across all 10 consecutive BRANCHes. Compare *distribution*,
not just mean — the v0.3.4 fix was a story about tail behavior.

### Phase 4: hardening (Week 6-7)

Edge cases to specifically test:

- Write-heavy guest (`stress-ng --vm 1 --vm-bytes 90%` running inside).
- NUMA cross-node guest RAM (force memfd allocations across nodes).
- Concurrent BRANCHes on different parents (shared uffd handler thread
  pool? Or one handler per BRANCH?).
- Kernel < 5.7 (no `UFFD_WP`) — graceful detection + fallback to
  v0.3.4 pause-based path.
- THP enabled/disabled.
- Memory pressure during BRANCH (host actively swapping).

### Phase 5: launch (Week 8)

- Switch `--live-fork` to default-on after a stabilization pass.
- Write up the implementation as a post-mortem-style article (same
  cadence as the v0.3.4 ext4 story).
- Ship v0.4.
- File any upstream kernel/Firecracker issues discovered along the way.

## Risks

- **Kernel < 5.7 doesn't have `UFFDIO_WRITEPROTECT`.** Mitigation:
  detect at startup, fall back to v0.3.4 path, document minimum
  supported kernel. Ubuntu 20.04 LTS has 5.4 — that's a real
  deployment hit. Possible workaround: backport detection so 5.4
  users transparently get v0.3.4 behavior.

- **Write-fault storms.** A guest scribbling all of RAM during BRANCH
  generates one fault per page. At 4 KiB pages × 1 GiB RAM that's
  262,144 faults. Each fault is microseconds of kernel + userspace
  work; bound is ~1 s to drain — *worse* than v0.3.4 pause for this
  pathological case. Mitigation: measure, document the regime, add
  a "give up, fall back to pause" escape hatch when fault rate
  exceeds threshold.

- **Snapshot consistency under uffd_wp ordering.** Need careful proof
  that the snapshot represents a consistent point-in-time even with
  async page copying. Plan: write a model + property test using
  `loom` or similar to fuzz the page-state machine.

- **Restore-time regression.** The new snapshot format (if it ends up
  different from v0.3.4) might restore slower. Need to bench both
  paths under the same workload before declaring v0.4 a win
  end-to-end.

## References

- Linux kernel docs: `Documentation/admin-guide/mm/userfaultfd.rst`
- `userfaultfd(2)`, `ioctl_userfaultfd(2)` man pages
- CRIU lazy-migration implementation:
  [github.com/checkpoint-restore/criu](https://github.com/checkpoint-restore/criu)
  (especially `criu/lib/uffd.c`)
- Firecracker UFFD restore support:
  [github.com/firecracker-microvm/firecracker](https://github.com/firecracker-microvm/firecracker)
  (`src/vmm/src/persist.rs`)
- "Live Migration of Virtual Machines" — Clark et al., NSDI 2005
  (the original pre-copy paper, for the alternative-design comparison)
- forkd v0.3.4 ext4 fix retrospective:
  [`bench/pause-window/PROBE-multi-branch-anomaly.md`](./bench/pause-window/PROBE-multi-branch-anomaly.md)
- Tracking issue:
  [#101](https://github.com/deeplethe/forkd/issues/101)
