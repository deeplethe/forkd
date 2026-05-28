# forkd's vendored Firecracker

**Status:** active until [`firecracker-microvm/firecracker#5912`](https://github.com/firecracker-microvm/firecracker/issues/5912) lands upstream.

## What and why

forkd v0.4's live-fork primitive needs `MAP_SHARED` on the snapshot-restore memory mapping so that an external snapshot manager — `forkd-controller`, in our case — can observe guest memory writes through its own `mmap` of the same memfd. The current upstream Firecracker hardcodes `MAP_PRIVATE` in `src/vmm/src/vstate/memory.rs::snapshot_file`; without `MAP_SHARED`, guest writes copy-on-write into FC's private pages and never propagate back to the controller's view.

The fix is a ~33-line patch adding an opt-in `shared: bool` field to `MemBackendConfig`. The patch is filed upstream as [`firecracker#5912`](https://github.com/firecracker-microvm/firecracker/issues/5912) (proposal docs: [`FIRECRACKER-UPSTREAM-PROPOSAL.md`](../FIRECRACKER-UPSTREAM-PROPOSAL.md), full diff: [`0001-feat-mem-backend-shared-option-for-MAP-SHARED.patch`](../0001-feat-mem-backend-shared-option-for-MAP-SHARED.patch)). While that upstream conversation runs, forkd vendors the patch on a Firecracker fork so v0.4 isn't blocked.

## Where the fork lives

[**`deeplethe/firecracker`**](https://github.com/deeplethe/firecracker), branch [`forkd-v0.4-mem-backend-shared`](https://github.com/deeplethe/firecracker/tree/forkd-v0.4-mem-backend-shared).

- Forked from upstream `main` at the commit cited in the proposal.
- One commit on the branch: the proposal's diff applied verbatim. No other deviations from upstream.
- The branch will be rebased onto upstream `main` periodically until upstream lands its own version of the patch (or we hear back that they don't want it; in which case the fork becomes our permanent home for it).

## When upstream lands the patch

If [`firecracker#5912`](https://github.com/firecracker-microvm/firecracker/issues/5912) merges, we delete the `forkd-v0.4-mem-backend-shared` branch on our fork and point forkd's build path back at upstream. The fork stops being a maintenance burden the moment upstream has the equivalent.

If upstream declines or never responds (the [historical pattern for FC's external-tool features](https://github.com/firecracker-microvm/firecracker/issues/5768) — sparse-memory snapshots was closed as "use existing diff snapshots") then the fork keeps running and forkd documents its FC dependency as "deeplethe/firecracker, not vanilla."

## Building the patched binary

```bash
git clone -b forkd-v0.4-mem-backend-shared https://github.com/deeplethe/firecracker.git
cd firecracker
tools/devtool build --release
# Binary lands at: build/cargo_target/x86_64-unknown-linux-musl/release/firecracker
```

Build prerequisites are the same as upstream Firecracker (Docker + tools/devtool).

## Smoke-checking the patch worked

`firecracker-patch/test-shared.sh` (in `space/` next to `forkd/`) is a 30-line bash script that loads an existing forkd snapshot with the patched binary, walks `/proc/<fc_pid>/maps`, and reports whether the `memory.bin` mapping is `rw-s` (MAP_SHARED — patch active) vs `rw-p` (MAP_PRIVATE — unpatched FC).

Expected output:

```
shared=false → rw-p   # MAP_PRIVATE (unchanged FC behavior)
shared=true  → rw-s   # MAP_SHARED (what the patch enables)
```

## What forkd needs to do next

The patched FC binary is necessary but not sufficient for v0.4. Three remaining pieces of forkd-side work, each scoped as a separate PR sequence:

1. **`forkd-vmm` memfd-backed spawn path** (Phase 5a, ~1 week). `forkd-controller` `memfd_create()`s a region, populates it with the snapshot's `memory.bin`, then hands FC `/proc/self/fd/<N>` as `mem_backend.backend_path` with `shared: true`. Tracking issue to be filed.
2. **Default-on memfd backing on supported kernels** (Phase 5b, ~3 days). Auto-detects kernel ≥ 5.7 with `uffd_wp`-on-shmem support; falls back to file-backed otherwise. See `forkd doctor` checks 15-16 in [`DESIGN-v0.4-USER-API.md`](../DESIGN-v0.4-USER-API.md).
3. **`mode="live"` BRANCH path** (Phase 6, ~2 weeks). The asynchronous dirty-page copier — the actual live-fork primitive. See [`DESIGN-v0.4.md`](../DESIGN-v0.4.md) for the kernel mechanics.

Phases 5a → 5b → 6 are sequential. 7-9 (REST/CLI/SDK plumbing, doctor checks, benchmarks) are mostly parallel once the runtime path works.

## Risk: upstream rebase conflicts

The patch touches three files in `src/vmm/src/`:
- `persist.rs` — adds a `shared` parameter through `guest_memory_from_file`
- `vmm_config/snapshot.rs` — adds the `shared` field to `MemBackendConfig`
- `vstate/memory.rs` — chooses `MAP_SHARED` vs `MAP_PRIVATE` based on the flag

These are stable interfaces by FC standards. Upstream rebases should be clean unless FC restructures the snapshot path itself, which their release notes haven't signaled. If a rebase ever fails, the patch is small enough to re-apply by hand in ~10 minutes.
