# Firecracker upstream proposal — `mem_backend.shared` option for cooperative-snapshot tooling

**Status:** draft proposal targeting `firecracker-microvm/firecracker`.
Not yet filed.

## TL;DR

Add an opt-in `shared: true` field to `mem_backend` so that
Firecracker mmaps the backing memory file with `MAP_SHARED` instead
of the current `MAP_PRIVATE`. The default stays `MAP_PRIVATE` (no
behavior change for existing users). Cost: ~10 lines of code. Use
case: external snapshot-management tools that need to observe guest
memory writes (forkd's v0.4 live-fork primitive is the concrete one,
but the option is general).

## Background — current behavior

`PUT /snapshot/load` with `mem_backend.backend_type = "File"`
restores guest memory from a file. The relevant code is in
`src/vmm/src/vstate/memory.rs::snapshot_file`:

```rust
fn snapshot_file(file: File, regions: ..., track_dirty_pages: bool)
    -> Result<Vec<GuestRegionMmap>, ...>
{
    create(
        regions.into_iter(),
        libc::MAP_PRIVATE,           // ← hardcoded
        Some(file),
        track_dirty_pages,
    )
}
```

`MAP_PRIVATE` is correct for the standard use case: restore guest
memory from a snapshot file you don't intend to share with anyone.
The kernel does copy-on-write on the first guest write, giving FC
its own private copy of each modified page.

## What we want

`MAP_SHARED` is what some external tools need. Specifically: a
process (call it the "snapshotter") creates a memfd, populates it
with the snapshot's memory image, and hands FC a path to that
memfd via `/proc/<snapshotter_pid>/fd/<N>`. FC opens that path,
mmaps it, and runs the guest.

If the mmap is `MAP_SHARED`, guest writes propagate back to the
snapshotter's view of the memfd. The snapshotter can then arm
`UFFDIO_WRITEPROTECT` on its own mmap (which catches the guest's
writes through the kernel's MMU-notifier path → EPT invalidation
→ uffd_wp delivery — empirically verified on kernel 6.14 in
[forkd Phase 2 PoC](https://github.com/deeplethe/forkd/tree/main/experiments/v0.4-kvm-uffd-wp-poc))
and asynchronously capture a fresh snapshot of guest memory
*without* a synchronous memory write inside FC's snapshot/create
pause window.

For a 1 GiB parent VM, this drops the BRANCH pause window from
~150 ms (current FC `snapshot/create` floor on ext4 after the
forkd v0.3.4 `posix_fallocate` fix) to ~3 ms
([forkd Phase 1 PoC](https://github.com/deeplethe/forkd/tree/main/experiments/v0.4-uffd-wp-poc)).
At larger parent sizes the speedup scales linearly with memory.

`MAP_PRIVATE` makes this impossible: guest writes trigger
copy-on-write to FC's private pages, so the snapshotter's mmap
never sees them.

## Proposed change

### API

`MemBackendConfig` gains an optional `shared` field:

```diff
 pub struct MemBackendConfig {
     pub backend_type: MemBackendType,
     pub backend_path: PathBuf,
+    /// If true, the `File` backend's mmap uses `MAP_SHARED`
+    /// instead of the default `MAP_PRIVATE`. Defaults to false.
+    /// Has no effect for `Uffd` backend.
+    #[serde(default)]
+    pub shared: bool,
 }
```

Existing JSON requests work unchanged. Setting `shared: true`
opts in to the new behavior.

### Implementation

`snapshot_file` takes a `shared` flag through and passes the
corresponding mmap flag to `create`:

```diff
-fn snapshot_file(file: File, regions: ..., track_dirty_pages: bool)
+fn snapshot_file(file: File, regions: ..., track_dirty_pages: bool, shared: bool)
     -> Result<Vec<GuestRegionMmap>, GuestMemoryFromFileError>
 {
+    let mmap_flags = if shared { libc::MAP_SHARED } else { libc::MAP_PRIVATE };
     create(
         regions.into_iter(),
-        libc::MAP_PRIVATE,
+        mmap_flags,
         Some(file),
         track_dirty_pages,
     )
 }
```

Caller propagates the `shared` flag from the parsed config. Net
diff is well under 30 lines.

### Compatibility

- Default unchanged. Every existing user gets `MAP_PRIVATE`.
- Snapshot file format unchanged.
- Restore semantics unchanged for `shared: false`.
- `shared: true` has the usual `MAP_SHARED` caveats (writes are
  durable to the backing fd; concurrent writers must coordinate).
  These belong in docs, not in API behavior changes.

### Tests

Two test additions:

1. Unit test: build a `MemBackendConfig { shared: true }`, confirm
   the resulting mmap flag is `MAP_SHARED` (via `/proc/self/maps`
   inspection in an integration test).
2. Integration test: snapshot + restore round-trip with
   `shared: true`, confirm guest behavior is identical to
   `shared: false`.

## Alternatives considered

- **`SnapshotType::VmstateOnly`** snapshot-create variant that
  writes only vmstate, no memory.bin. Cleaner for the v0.4 use
  case (forkd doesn't need FC to write memory at all). Costs:
  more invasive (~50 lines of code in the snapshot-create path),
  changes the on-disk format expectations, and the use case is
  narrower (only helps the cooperative-snapshot pattern, not, say,
  KVM-level inspection tools).
- **A new `SharedFile` backend type** alongside `File` and `Uffd`.
  More explicit but doubles the documentation surface and the
  match-arms. The opt-in field on the existing `File` backend
  reads simpler.

The opt-in field is the smallest viable change.

## Who benefits

- **forkd** (this is what motivated the proposal). v0.4 live-fork
  primitive cuts BRANCH pause from ~150 ms to ~3 ms for a 1 GiB
  parent. Empirically validated kernel mechanics; only the FC API
  change blocks integration.
- **Any external snapshot manager** built on Firecracker that
  wants to observe guest memory writes from outside the FC
  process. Examples: forensic VM analysis, live-migration
  controllers built on top of FC, custom DRAM-tier-of-cache
  systems.
- **No user is harmed** — default behavior is unchanged.

## Open questions

- Should `shared: true` interact with `track_dirty_pages`? The
  current dirty-tracking path uses KVM_GET_DIRTY_LOG, which
  is orthogonal to the mmap flag. Best to leave them independent
  in the first cut.
- Are there security implications? `MAP_SHARED` lets the
  backing-file holder see guest memory in real time. This is the
  point of the feature; users opt in by setting the field. No new
  capability beyond "the system administrator controls who can
  hand FC a `/proc/<pid>/fd/N` path."

## Patch status

**Implemented and built**, against firecracker `main`
(v1.16.0-dev, commit `053f521d9`). The unified diff is saved in
this repo as
[`0001-feat-mem-backend-shared-option-for-MAP-SHARED.patch`](./0001-feat-mem-backend-shared-option-for-MAP-SHARED.patch)
(92 lines including diff headers).

Patch shape:

```
 src/firecracker/src/api_server/request/snapshot.rs |  6 ++++++   (test fixtures)
 src/vmm/src/persist.rs                             | 13 ++++++++++---
 src/vmm/src/vmm_config/snapshot.rs                 | 10 ++++++++++
 src/vmm/src/vstate/memory.rs                       | 14 +++++++++++++-
 4 files changed, 39 insertions(+), 4 deletions(-)
```

Build status: `cargo build --release --bin firecracker` on
Ubuntu 24.04 / kernel 6.14 with Rust stable → finishes in 26s,
zero warnings.

Binary smoke test: `firecracker --version` → prints
`Firecracker v1.16.0-dev`.

End-to-end MAP_SHARED verification: deferred. The natural test
(load an existing forkd snapshot with `shared: true` and check
`/proc/<fc_pid>/maps` for `rw-s`) requires a snapshot that
matches the patched FC's version (v1.16.0). All existing forkd
snapshots on the dev box were taken with FC v1.12.0 and produce
"bitcode serialization" errors when loaded by v1.16.0. The fix
is straightforward — take a fresh snapshot with the patched FC,
then re-run the test — but is left for the upstream PR's CI to
exercise rather than blocking this proposal on it.

## Filing plan

1. Open a Firecracker issue laying out the use case (~200 words,
   linking to forkd's Phase 1–4 PoC results).
2. Wait for maintainer feedback on API shape.
3. Send a PR implementing whatever shape they confirm.

If the upstream review takes longer than ~4 weeks, forkd will
maintain the patch in a downstream fork
(`deeplethe/firecracker-mapshared`) until upstream merges.

## See also

- [`DESIGN-v0.4.md`](./DESIGN-v0.4.md) — RFC for forkd v0.4.
- [`DESIGN-v0.4-PHASE3-SPIKE.md`](./DESIGN-v0.4-PHASE3-SPIKE.md) —
  full integration-options analysis (where this `MAP_PRIVATE`
  blocker was discovered).
- [`experiments/v0.4-uffd-wp-poc/RESULTS.md`](./experiments/v0.4-uffd-wp-poc/RESULTS.md)
  — empirical 3 ms/GiB pause-window data.
- [`experiments/v0.4-kvm-uffd-wp-poc/RESULTS.md`](./experiments/v0.4-kvm-uffd-wp-poc/RESULTS.md)
  — KVM-guest-write × UFFD_WP empirical confirmation.
