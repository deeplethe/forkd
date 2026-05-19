# Diff snapshots for forkd v0.3

**Status:** v0.3 phase 1. Design draft.
**Tracking:** ROADMAP.md → "Cut pause-window without forking Firecracker".
**Depends on:** Firecracker v1.10.1 (already in use); `track_dirty_pages: true` on `/machine-config` (already set).

## Goal

Reduce BRANCH pause-window from "write full memory.bin" (4 s for
513 MiB on SATA SSD, 1.04 s for 4 GiB on tmpfs) to "write only the
pages dirtied since the previous snapshot". Expected 5–10× win for
typical agent workloads where source has touched <10 % of its RAM
between BRANCHes.

## Mechanism

Firecracker's `PUT /snapshot/create` already accepts
`snapshot_type: "Diff"`. With `track_dirty_pages` enabled on
`/machine-config` (forkd does this by default since v0.1.x), the
guest kernel's per-page dirty bits flow into a sparse file:

- `Full` snapshot: writes every page (`mem_size_mib` × 1 MiB).
- `Diff` snapshot: writes a sparse file the same logical size as the
  full memory image, but with `lseek(SEEK_HOLE)` gaps over clean
  pages. Only dirtied pages get bytes; the rest is holes.

After a `Diff` snapshot, the dirty bitmap is **cleared**. The next
`Diff` snapshot starts from a fresh bitmap and only writes what's
been dirtied since the last snapshot.

## The reconstruction problem

Children need a memory.bin that reflects the source's **current**
state at BRANCH-pause time, not the source's BOOT state. A diff file
alone isn't enough — children would see boot state for any page
not in the diff.

Two sub-options:

### A. Per-sandbox shadow file (chosen)

When the daemon creates a source sandbox, it `cp`s the source tag's
`memory.bin` into a per-sandbox shadow file. The source firecracker
still mmaps the original tag's memory.bin (read-only path), but the
daemon now has a writable copy it can merge diffs into.

At each BRANCH:
1. Pause source (fast).
2. `snapshot/create` with `snapshot_type: "Diff"` → writes a sparse
   `diff-<timestamp>.bin` containing only the pages dirtied since
   the last snapshot.
3. Resume source.
4. Background thread merges `diff-<timestamp>.bin` onto the shadow
   file: walks the sparse file, copies non-hole pages into the
   shadow at the same offsets. Cost O(dirty page bytes).
5. Children spawn and `mmap(shadow_file, MAP_PRIVATE)` for guest RAM.

**Pause-window cost is O(dirty pages)**, not O(memory size). For a
typical agent that's touched 50 MB of working set, pause drops from
4 s (full write of 4 GiB on SATA) to ~60 ms (diff write of 50 MB).

**Sandbox-creation cost** gains one full memory.bin copy upfront.
On SATA this is ~4 s for 513 MiB or ~30 s for 4 GiB — amortized
across however many BRANCHes the source produces. Break-even is
**2 BRANCHes** even for the worst-case (full-memory) source diff;
typical agent workloads break even on the **first** BRANCH because
the diff is much smaller than the full memory.

### B. Direct merge inside BRANCH (rejected)

Could merge diff onto shadow synchronously inside BRANCH, before
returning. Doubles the pause-window over option A. Rejected.

### C. No shadow file, children layer base+diff at restore (rejected)

Have children mmap the source tag's `memory.bin` as base and the
diff file separately, with a custom page-fault handler that picks
base or diff per address. This is essentially what we deferred in
[`docs/design/userfaultfd.md`](./userfaultfd.md) — requires a uffd
handler and either a Firecracker patch or a clever userspace dance.
Phase 1 stays on the simple `MAP_PRIVATE` path; this is a candidate
for phase 2 if shadow-file storage becomes a bottleneck.

## Bitmap lifecycle and the "first BRANCH" question

Firecracker's dirty bitmap starts populated at restore time —
every page that's been touched since the snapshot was loaded counts
as "dirty". For a freshly-restored source that hasn't executed
anything, the bitmap is empty (no pages dirtied yet). For a source
that's been running, the bitmap reflects everything touched since
boot OR since the last snapshot.

So the FIRST BRANCH on a long-running source can have a near-full
dirty set, making Diff degrade to Full performance. This is
acceptable: the worst case is the existing v0.2 cost; subsequent
BRANCHes will be fast because the bitmap was cleared.

The "freshly restored source, immediately BRANCH" case is best:
dirty bitmap empty, Diff writes ~0 bytes, pause-window is just
the API round-trip plus vCPU state save (sub-100 ms).

## Implementation surface

### `forkd-vmm`

```rust
impl Vm {
    /// Existing.
    pub fn snapshot_to(&self, vmstate: PathBuf, memory: PathBuf, ...) -> Result<Snapshot>;

    /// NEW: write a Diff snapshot. memory_diff is a sparse file the
    /// same logical size as the source's mem_size_mib; only the
    /// dirtied pages are written. Caller is responsible for merging
    /// the diff onto a base before any restore.
    pub fn snapshot_diff_to(&self, vmstate: PathBuf, memory_diff: PathBuf, ...) -> Result<DiffSnapshot>;
}

pub struct DiffSnapshot {
    pub vmstate: PathBuf,
    pub memory_diff: PathBuf,
    /// `memory_diff` is logically this size; physically it's
    /// (count of dirty pages × page_size).
    pub logical_size_bytes: u64,
    pub volumes: Vec<VolumeSpec>,
}

/// Merge a diff sparse file onto a base memory.bin in place.
/// Copies non-hole pages from `diff` into `base` at the same offsets.
/// Returns the number of bytes copied (= dirty page bytes).
pub fn apply_diff(diff: &Path, base: &Path) -> Result<u64>;
```

### `forkd-controller`

`AppState` keeps a per-sandbox shadow file path. `branch_sandbox`
handler:

1. If sandbox has no shadow yet (first BRANCH after sandbox creation),
   the shadow file *is* the source tag's memory.bin and we degrade
   to Full snapshot.
2. Otherwise:
   - Call `Vm::snapshot_diff_to(diff_file)`.
   - Spawn background task that applies the diff onto the shadow.
   - Update the source's tag's `memory.bin` reference to point at the
     shadow file for the children's restore.
   - Children's `mem_file_path` is the shadow file, MAP_PRIVATE.

For v0.3 phase 1a (MVP) we DON'T thread the shadow path through the
API — we measure the diff snapshot mechanism in isolation against
the existing Full path. Phase 1b wires the shadow file.

## Measurement plan

Reuse `bench/pause-window/sweep-prewarm.sh` shape. New experiment:

- For each memory size: spawn source, BRANCH 5 times in a row.
- prewarm=false, diff=false → today's behavior (baseline).
- prewarm=false, diff=true → diff snapshots.
- Measure `pause_ms` per BRANCH. Expect BRANCH 1 ≈ baseline (cold
  dirty set), BRANCH 2–5 ≪ baseline (small diffs).

Publish in `bench/pause-window/RESULTS-v0.3.md`.

## Out of scope for phase 1

- Diff-of-diff (BRANCH N+1's diff is relative to BRANCH N's shadow,
  not BRANCH N's diff). Phase 1's shadow-file merge handles this
  transparently.
- Cross-host diff transport (interesting for the Hub but separate).
- Compression of diff files before merge (zstd; useful for storage,
  not for pause-window).

## Phasing

| Phase | Scope | Status |
|---|---|---|
| 1a | `Vm::snapshot_diff_to` + `apply_diff` + unit tests + measurement on isolated source. | **Landed.** |
| 1b | `branch_sandbox` with `diff: true` mode (parallel cp + diff during pause + apply on resume). Initially restricted to first BRANCH per sandbox. | **Landed.** |
| 1c | Bench `sweep-diff-real.sh` + `sweep-agent.sh` + RESULTS-v0.3.md threshold curve. | **Landed.** |
| 1d | **Multi-BRANCH diff via the previous-BRANCH-output chain.** No separate shadow file: each BRANCH's `memory.bin` is, by construction, source's state at that BRANCH's pause time — exactly the base the next diff needs. Tracked as `SandboxInfo.last_branch_memory_path`. Falls back to source-tag (with a warning) if the user deletes an intermediate BRANCH. | **Landed.** |

## Multi-BRANCH diff: the previous-output chain (phase 1d)

Firecracker's dirty bitmap is cleared on EVERY `snapshot/create`,
Full or Diff. So once any BRANCH has been taken from a sandbox, the
NEXT diff would only see pages dirtied between BRANCH N and
BRANCH N+1 — missing everything dirtied before BRANCH N. If we
applied that diff onto `source_tag/memory.bin` (the boot state),
the resulting snapshot would be missing N batches of dirty pages.

**Key insight:** each successful BRANCH's `snap_dir/memory.bin` is,
by construction, the source's complete memory state at that BRANCH's
pause time. That's already the base the next diff needs. **The
"shadow file" we'd otherwise have to maintain is just the previous
BRANCH's output.**

Phase 1d implementation:

- `SandboxInfo.last_branch_memory_path: Option<PathBuf>` tracks
  whichever BRANCH most recently completed (Full or Diff — both
  clear the bitmap, so either works as the next chain head).
- On every successful BRANCH, the daemon calls
  `Registry::mark_branched(id, snap_dir/memory.bin)` to update the
  chain head.
- On every `diff: true` request, the daemon picks the cp source as
  the chain head if set AND the file still exists. Otherwise (first
  BRANCH on a sandbox, or chain broken by user deletion) it falls
  back to `source_tag/memory.bin` with a logged warning.

Trade-offs:

- **Zero extra storage cost.** Each BRANCH's output already lives
  in `<snapshot_root>/<tag>/memory.bin`; we just point at it.
- **No background tasks.** No shadow-update thread, no JoinHandle
  bookkeeping. The chain head is just metadata.
- **Chain breaks cleanly on deletion.** If the user `DELETE`s a
  BRANCH snapshot whose `memory.bin` is the current chain head,
  the next diff BRANCH degrades to "fall back to source tag"
  with a warning. This is semantically lossy (loses pages dirtied
  before deletion) but doesn't crash; the operator can choose to
  switch to Full mode for that BRANCH if they need correctness.

Lifted in v0.3.1. The previously-shipped `has_branched: bool` flag
stays in `SandboxInfo` as a diagnostic but the daemon no longer
gates on it.
