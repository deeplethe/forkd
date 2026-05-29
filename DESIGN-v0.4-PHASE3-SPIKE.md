# v0.4 Phase 3 spike — Firecracker integration options

**Status:** RESOLVED — picked **Option B + light parallel Option A**. The forkd-patched Firecracker fork lives at [`deeplethe/firecracker:forkd-v0.4-mem-backend-shared-v1.12`](https://github.com/deeplethe/firecracker/tree/forkd-v0.4-mem-backend-shared-v1.12) (3 commits on top of the v1.12.0 tag: main patch + struct-literal build fix + O_RDWR fix). See [`docs/VENDORED-FIRECRACKER.md`](./docs/VENDORED-FIRECRACKER.md) for the operational details (build, rebase plan, the earlier `forkd-v0.4-mem-backend-shared` v1.16-dev branch kept around for upstream-PR archeology). Upstream conversation tracked at [`firecracker-microvm/firecracker#5912`](https://github.com/firecracker-microvm/firecracker/issues/5912) — filed 2026-05-25, currently awaiting first-response (FC's typical SLA for external feature requests is 30-90 days, per [#5768 precedent](https://github.com/firecracker-microvm/firecracker/issues/5768)). The vendored fork decouples forkd v0.4 progress from upstream timing.

---

(Original spike text below, kept for the decision trail.)

The library API (`WpBranch::begin/bulk_copy_clean/finalize`) and its
CLI surface (`forkd wp-bench`) are merged on main as of [#157]. The
remaining work is replacing v0.3.4's "FC writes memory.bin
synchronously inside the pause" with v0.4's "WP-arm in pause, FC
writes only vmstate, forkd captures memory async."

## The blocker

[Firecracker's `snapshot_memory_to_file`][fc-snapshot] does:

```rust
let mut file = OpenOptions::new()
    .write(true).create(true).truncate(false).open(mem_file_path)?;
file.set_len(expected_size)?;       // ← the killer
match snapshot_type {
    SnapshotType::Diff => guest_memory.dump_dirty(&mut file, &dirty_bitmap),
    SnapshotType::Full => guest_memory.dump(&mut file),
}
```

`set_len` on the destination file means `mem_file_path` must be a
regular file — character devices like `/dev/null` reject it. FC also
always writes memory contents (whole region for `Full`, dirty pages
for `Diff`) before returning. There is no `VmstateOnly` snapshot
type.

So for v0.4, we cannot just point FC's snapshot output at the void
and use WpBranch for memory. FC will write memory; the question is
where and how to make that write cheap.

## Options

### A. Patch Firecracker upstream — add `SnapshotType::VmstateOnly`

Add a new variant that skips the memory dump entirely. ~20 lines of
FC source. Sent to upstream as a PR, this is the right long-term
answer. But: FC's release cadence is monthly and breaking-change
review is slow; we'd be downstream-vendoring FC for at least a quarter
before this lands.

**Effort:** 1 day code + weeks/months for upstream merge.
**Risk:** Low (clean enhancement, no behavior change for existing users).

### B. Vendor a forkd-patched Firecracker

Same as A but maintain the patch in a fork in this repo. Avoids the
upstream wait. Cost: rebase the patch against FC `main` every release;
forkd's `scripts/build-rootfs.sh` and CI need to pull the fork's
artifact instead of the upstream binary.

**Effort:** 1 day initial + ongoing rebase tax.
**Risk:** Drift between forkd-FC and upstream FC features; users who
want to BYO their own FC binary lose the v0.4 path.

### C. Bypass FC's snapshot/create entirely

The vmstate that FC serializes is per-vCPU `kvm_regs` + `kvm_sregs` +
device state + kvmclock + TSC offset, all reachable via KVM ioctls on
the VM fd. If forkd-controller can get a handle to FC's VM fd, it
could pause + read vmstate + resume without going through FC's HTTP
API.

How forkd-controller gets FC's VM fd: not easy. FC owns the fd
internally; the only ways out are `ptrace` on the FC process or
having FC explicitly share the fd at startup. Neither is clean.

**Effort:** 1+ weeks (need to learn FC's internal vmstate format
to serialize it ourselves).
**Risk:** High — vmstate format is internal to FC, changes without
notice, and reproducing the on-disk format means restore-compatibility
becomes our problem.

### D. tmpfs + discard

Point `mem_file_path` at `/dev/shm/forkd-discarded-<id>.bin` so the
FC write goes to RAM at ~RAM throughput (~2 GB/s on commodity DDR),
then unlink immediately after. Forkd separately uses WpBranch to
capture memory into the real snapshot.

Cost: the FC write still happens *inside* the pause window. For
a 1 GiB parent, ~500 ms inside the critical section (tmpfs is faster
than ext4's 150 ms? Actually tmpfs is faster — but only when the
working set fits in RAM, and writing 1 GiB to tmpfs allocates 1 GiB
of RAM). Net pause is `arm_WP + tmpfs_write + small_overhead` ≈
500 ms + 3 ms. **Worse than v0.3.4** for the parent-pause metric.

Wait — there's a nuance. For `Diff` mode, FC's `dump_dirty` writes
only the dirty pages, not the whole region. If the source VM is
clean immediately before BRANCH (which the agent fan-out pattern
permits — we can flush dirty bitmap right before BRANCH), the FC
write is tiny: maybe a few hundred KB. Diff-mode tmpfs discard
could keep the pause to ~50 ms total. That's a 3× improvement over
v0.3.4 with no FC patch.

**Effort:** 1-2 days.
**Risk:** Medium — depends on dirty bitmap being small at BRANCH time.
For idle parents this is fine; for actively-working parents it
degenerates back to v0.3.4 speeds.

### E. Pre-arm UFFD_WP, accept FC's existing write, dedupe

Arm UFFD_WP on FC's memory region before calling FC's snapshot/create.
FC's writes to its own memory don't pass through user-space (they
read memory and write to mem_file_path), so they won't fault. But
the kernel's MMU notifier path might invalidate EPT entries and slow
down vCPU on resume.

After FC's snapshot/create returns, we already have a full memory.bin
on disk. We don't need WpBranch's capture mechanism at all. The
benefit would be... none for the snapshot path. UFFD_WP only helps
if we use it to defer memory copy out of the pause window.

So E is incoherent. Skip.

## Recommendation

**Tier 1 (immediate, ~1 week)**: Path **D** with `Diff` mode and a
pre-BRANCH dirty bitmap flush. Gets v0.4 to ~50 ms pause for idle
parents without patching FC. Documents the regression for
write-heavy parents as a known limitation.

**Tier 2 (parallel, ~2 weeks)**: Path **A**, submit upstream FC patch
for `SnapshotType::VmstateOnly`. When it lands, switch forkd to use
it; pause window drops to ~3 ms (WpBranch arm only).

**Tier 3 (later, when needed)**: Path **C** if upstream rejects A and
we need sub-10 ms unconditionally.

## What's needed to start Tier 1

1. Add `mem_file_path = /dev/shm/forkd-discard-<id>.bin` plumbing in
   `forkd-vmm::snapshot_to_diff`.
2. After FC's snapshot/create returns, `unlink` the discard file
   (or `mmap` it for WP capture if we want overlap, but probably
   simpler not to).
3. Run WpBranch with the source VM's actual memory region to populate
   the real snapshot file.
4. Add a `--live-fork` feature flag in the controller's BranchSandbox
   request shape. Default off until benchmarked vs v0.3.4.
5. Reproduce the v0.3.4 multi-BRANCH bench
   (`bench/pause-window/sweep-diff.sh`) with `--live-fork` to confirm
   the pause-window claim.

## Newly discovered path: shared memfd via /proc/self/fd

Firecracker internally backs guest memory with `memfd_create` (verified
in `src/vmm/src/vmm_config/machine_config.rs`). At restore time, the
`PUT /snapshot/load` API accepts a `mem_backend` field with
`backend_type ∈ {File, Uffd}` and a `backend_path`.

If forkd creates its own memfd, mmaps it for WP-arming, then passes
`/proc/<forkd-pid>/fd/<memfd_fd>` to FC as `backend_path` with
`backend_type=File`, FC opens that path (which resolves to the same
underlying memfd inode) and uses it as the restored VM's memory
backing. Both processes now hold shared access to the same memfd.

**If this works**, v0.4 integration doesn't require an FC patch:

1. forkd creates a memfd, mmaps it.
2. forkd loads the parent snapshot into the memfd (just memcpy from
   the old memory.bin into the mmap).
3. forkd passes `/proc/self/fd/<N>` to FC at restore time.
4. FC restores the VM normally; both processes see the same memory.
5. At BRANCH time, forkd arms `UFFDIO_WRITEPROTECT` on its mmap.
   FC's guest writes still trap to uffd (verified in Phase 2 PoC —
   EPT-mediated writes do propagate to UFFD_WP on the host VMA).
6. forkd calls FC's snapshot/create with `mem_file_path=/dev/shm/discard`
   (small tmpfs). FC writes vmstate normally + memory contents go to
   tmpfs (wasted but cheap on tmpfs).
7. WpBranch captures the real snapshot via its handler thread.
8. forkd unlinks the discard file.

The remaining cost in the pause window: FC writes guest memory to
tmpfs. For a 1 GiB parent that's ~500 ms (RAM throughput). Diff mode
+ dirty bitmap flush before BRANCH would reduce this dramatically
(only dirty bytes get written).

This requires testing whether MAP_SHARED on FC side propagates writes
back to forkd's mmap. If FC uses MAP_PRIVATE for restored memory, the
two processes have divergent views and v0.4 fails. Phase 4 PoC
should test this.

### Update (2026-05-25 afternoon): /proc/self/fd path is DEAD

A direct read of Firecracker's `src/vmm/src/vstate/memory.rs::snapshot_file`
shows that the `File` backend mmaps `mem_backend.backend_path` with
**`MAP_PRIVATE`**, not `MAP_SHARED`:

```rust
create(
    regions.into_iter(),
    libc::MAP_PRIVATE,
    Some(file),
    track_dirty_pages,
)
```

This was empirically confirmed in
[`experiments/v0.4-memfd-share-spike/map-private-test.py`](./experiments/v0.4-memfd-share-spike/map-private-test.py):
process B (FC analog) opens process A's memfd via
`/proc/<A>/fd/<N>`, mmaps `MAP_PRIVATE`, writes a pattern; process A
re-reads and sees the *unchanged* content — B's writes did not
propagate.

So even if FC accepts `/proc/<forkd_pid>/fd/<N>` as its
`mem_backend.backend_path`, the resulting mapping is `MAP_PRIVATE`,
which gives forkd→FC propagation (the snapshot loads into FC) but
not FC→forkd (guest writes don't reach forkd's mmap, so WpBranch
can't capture them). The /proc/self/fd path described above only
works in *one* direction; v0.4 needs both.

**Conclusion: v0.4 requires an FC upstream patch.** See
[`FIRECRACKER-UPSTREAM-PROPOSAL.md`](./FIRECRACKER-UPSTREAM-PROPOSAL.md)
for the minimal proposal we'd send.

### Empirical confirmation (memfd-share spike, 2026-05-25)

Verified on dev box (Ubuntu 24.04, kernel 6.14): a memfd created in
process A is openable by process B via `/proc/<A_pid>/fd/<N>`, and
writes through either fd are visible to both.

```
[parent] created memfd fd=3, pid=1331774
[stage 1] same-process re-open via /proc/self/fd: ok
[stage 2] child opened parent's memfd via /proc path
[stage 3] child wrote "WRITTEN_FROM_CHILD"
[parent] post-child read shows the child's write ✓
```

(Script: `experiments/v0.4-memfd-share-spike/spike.py`.)

The remaining unknown is whether Firecracker specifically opens
`mem_backend.backend_path` with `MAP_SHARED` (works for v0.4) or
`MAP_PRIVATE` (breaks v0.4). FC's existing `Uffd` backend uses
shared semantics for the registered uffd region; the `File` backend
should be similar but we have not verified empirically. Phase 5
PoC will boot a real FC against `/proc/self/fd/<N>` and confirm.

## Open questions for next session

- Can we ask FC for a vCPU pause without going through snapshot/create?
  (FC has `/vm` PATCH with `state: Paused` — does that work standalone?)
- If we pause FC then arm UFFD_WP then resume FC then capture, what's
  the ordering with vmstate serialization? Need to be careful that
  vmstate is captured *while paused* and matches the WP-arm point in
  time.
- Does FC's `Diff` mode reset the dirty bitmap as part of
  snapshot/create, or does that need a separate call? (Affects how
  often we need to flush before BRANCH.)

[#157]: https://github.com/deeplethe/forkd/pull/157
[fc-snapshot]: https://github.com/firecracker-microvm/firecracker/blob/main/src/vmm/src/vstate/vm.rs
