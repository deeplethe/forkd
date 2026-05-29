# forkd's vendored Firecracker

**Status:** active until [`firecracker-microvm/firecracker#5912`](https://github.com/firecracker-microvm/firecracker/issues/5912) lands upstream.

## What and why

forkd v0.4's live-fork primitive needs `MAP_SHARED` on the snapshot-restore memory mapping so that an external snapshot manager — `forkd-controller`, in our case — can observe guest memory writes through its own `mmap` of the same memfd. The current upstream Firecracker hardcodes `MAP_PRIVATE` in `src/vmm/src/vstate/memory.rs::snapshot_file`; without `MAP_SHARED`, guest writes copy-on-write into FC's private pages and never propagate back to the controller's view.

The fix is a ~44-line patch adding an opt-in `shared: bool` field to `MemBackendConfig`. The patch is filed upstream as [`firecracker#5912`](https://github.com/firecracker-microvm/firecracker/issues/5912) (proposal docs: [`FIRECRACKER-UPSTREAM-PROPOSAL.md`](../FIRECRACKER-UPSTREAM-PROPOSAL.md), full diff: [`0001-feat-mem-backend-shared-option-for-MAP-SHARED.patch`](../0001-feat-mem-backend-shared-option-for-MAP-SHARED.patch)). While that upstream conversation runs, forkd vendors the patch on a Firecracker fork so v0.4 isn't blocked.

## Where the fork lives

[**`deeplethe/firecracker`**](https://github.com/deeplethe/firecracker), branch [`forkd-v0.4-mem-backend-shared-v1.12`](https://github.com/deeplethe/firecracker/tree/forkd-v0.4-mem-backend-shared-v1.12) (matches the FC version forkd produces snapshots with).

- Forked from upstream `v1.12.0` tag.
- Three commits on the branch (combined ~44 lines):
  - `cc3632b72 feat(mem_backend): opt-in shared: true for MAP_SHARED snapshot restore` — main patch (API field + mmap-flag plumbing).
  - `f3b299ff7 fix(snapshot): add shared: false to MemBackendConfig literals (build fix)` — fills in `shared: false` at six unpatched struct-literal sites so the tree still compiles.
  - `fe2b39026 fix(snapshot): open mem_file with O_RDWR when shared=true` — without this, `mmap(..., MAP_SHARED, ...)` returns `EACCES` on a read-only fd.
- The earlier `forkd-v0.4-mem-backend-shared` branch (rebased on upstream `main`/v1.16-dev) is kept for the upstream PR diff; the v1.12 branch is the one forkd actually builds against.
- The branch will be rebased onto upstream `v1.12.x` (or whichever tag forkd snapshots with) until upstream lands its own version of the patch.

## When upstream lands the patch

If [`firecracker#5912`](https://github.com/firecracker-microvm/firecracker/issues/5912) merges, we delete both `forkd-v0.4-mem-backend-shared` branches on our fork and point forkd's build path back at upstream. The fork stops being a maintenance burden the moment upstream has the equivalent.

If upstream declines or never responds (the [historical pattern for FC's external-tool features](https://github.com/firecracker-microvm/firecracker/issues/5768) — sparse-memory snapshots was closed as "use existing diff snapshots") then the fork keeps running and forkd documents its FC dependency as "deeplethe/firecracker, not vanilla."

## Building the patched binary

```bash
git clone -b forkd-v0.4-mem-backend-shared-v1.12 https://github.com/deeplethe/firecracker.git
cd firecracker
tools/devtool build --release
# Binary lands at: build/cargo_target/x86_64-unknown-linux-musl/release/firecracker
```

Build prerequisites are the same as upstream Firecracker (Docker + tools/devtool). The branch builds clean — no manual patch application required.

## Smoke-checking the patch worked

[`scripts/dev/test-shared.sh`](../scripts/dev/test-shared.sh) loads an existing forkd snapshot with the patched binary once per `shared` flag value, walks `/proc/<fc_pid>/maps`, and reports whether the `memory.bin` mapping is `rw-s` (MAP_SHARED — patch active) vs `rw-p` (MAP_PRIVATE — unpatched FC). Requires an existing forkd snapshot at `~/.local/share/forkd/snapshots/coding-agent-fork-prewarm-v1` (override via `SNAP_DIR=…`) and the patched binary on `$FC_BIN`.

Expected output:

```
shared=false → rw-p   # MAP_PRIVATE (unchanged FC behavior)
shared=true  → rw-s   # MAP_SHARED (what the patch enables)
```

## What forkd needs to do next

The patched FC binary is necessary but not sufficient for v0.4.

- **Phase 5a — `forkd-vmm` memfd helper** (done, PR #186): `forkd-controller` `memfd_create()`s a region, populates it with the snapshot's `memory.bin`, exposes `/proc/self/fd/<N>` as a backend path.
- **Phase 5b — memfd in restore_many_with** (done, PR #187): per-child memfd + per-child JSON body with `"shared": true`, gated on `MemoryBackend::MemfdShared`.
- **Phase 5c — patch verification** (done, 2026-05-29): `test-shared.sh` confirms `shared: true` produces `rw-s` (MAP_SHARED) and `shared: false` still produces `rw-p` (MAP_PRIVATE) on the v1.12 patched binary.
- **Phase 6 — `mode="live"` BRANCH path** (next, ~2 weeks): the asynchronous dirty-page copier — the actual live-fork primitive. See [`DESIGN-v0.4.md`](../DESIGN-v0.4.md) for the kernel mechanics.

7–9 (REST/CLI/SDK plumbing, doctor checks, benchmarks) are mostly parallel once Phase 6 lands.

## Risk: upstream rebase conflicts

The patch touches four files:
- `src/vmm/src/persist.rs` — adds a `shared` parameter through `guest_memory_from_file`; opens `mem_file` with `O_RDWR` when `shared=true` so `mmap(MAP_SHARED, PROT_WRITE)` doesn't return `EACCES`.
- `src/vmm/src/vmm_config/snapshot.rs` — adds the `shared` field to `MemBackendConfig` (default `false`).
- `src/vmm/src/vstate/memory.rs` — chooses `MAP_SHARED` vs `MAP_PRIVATE` based on the flag.
- `src/firecracker/src/api_server/request/snapshot.rs` — request-parsing side of the new field (struct-literal sites get `shared: false` to keep compatibility).

These are stable interfaces by FC standards. Upstream rebases should be clean unless FC restructures the snapshot path itself, which their release notes haven't signaled. If a rebase ever fails, the patch is small enough to re-apply by hand in ~10 minutes.
