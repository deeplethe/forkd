# v0.5: diff snapshot chains

**Status:** DRAFT — first pass. Comments via PR review or
[`docs/ROADMAP.md` M2.1](./ROADMAP.md) tracking thread.
**Target:** v0.5, ~3 weeks (per ROADMAP M2.1 estimate).
**Done criteria** (lifted verbatim from ROADMAP.md):

1. `forkd snapshot diff --from <base-tag> --tag <new-tag>` produces a
   diff `< 100 MB` for an `apt install` / `pip install` delta.
2. Restore time on a 3-snapshot chain is within 10% of the base
   snapshot's restore time.
3. Snapshot Hub MVP (M1.2) understands chains: pulling a diff also
   pulls its parents.

## Motivation

Today, every snapshot is a self-contained `(memory.bin, vmstate,
rootfs.ext4)` triple — Full from the daemon's perspective, regardless
of whether the bytes overlap with another tag. Modifying a parent
("`pip install pandas`" on top of `python-numpy`) means re-running the
whole pipeline:

```
docker pull → rootfs build → boot → warm → snapshot
```

That's ~10 s of work and several GB of duplicated bytes on disk per
delta. For an iterative recipe maintainer, the loop is painful:
prototype-an-extra, re-snapshot, ship. The 5th iteration looks
exactly like the 4th plus a 12 MB pandas wheel — but it costs the same
10 s + several GB.

The win is **structural disk + iteration time**, not BRANCH pause
window. v0.3 / v0.4 already attacked the BRANCH pause path; this
attacks the *build* path.

This is different from the v0.3 BRANCH diff path
([`docs/design/diff-snapshots.md`](./docs/design/diff-snapshots.md)):

- v0.3 diff: pause a *running* sandbox, snapshot dirty pages since
  the last BRANCH, resume. Optimizes BRANCH pause window.
- v0.5 chains: at *build* time, derive a new tag from an existing
  one by booting it, running an installer, snapshotting only the
  diff against the base. Optimizes build time + disk.

The same underlying FC primitive (`snapshot_type: "Diff"` +
`track_dirty_pages`) powers both; the difference is when the diff is
taken and how it's stored.

## Goal

`forkd snapshot diff --from python-numpy --tag python-numpy+pandas
--exec "pip install pandas"`:

1. Boots the `python-numpy` parent in a one-shot sandbox.
2. Runs the in-guest installer (`exec` against the guest agent).
3. Pauses, takes a Diff snapshot vs. the base.
4. Registers `python-numpy+pandas` with `parent_tag: python-numpy`
   in the registry.
5. On restore, the controller walks `python-numpy+pandas → python-numpy`
   and reconstructs memory by overlaying the diff onto the base.

End state: a 12 MB pandas-delta tag on disk; a 3-level chain
(`python-numpy → +pandas → +sklearn`) costs ~24 MB instead of
4.5 GiB.

## Non-goals

- **Compressing the *guest filesystem* diff.** Out of scope: rootfs.
  Chains apply to `memory.bin` only. The rootfs gets the apt /
  pip-installed bytes via the install step's writes to ext4; we keep
  copying the rootfs whole. Filesystem CoW (overlayfs / btrfs) is a
  separate piece of work, not bundled here.
- **Cross-base diffs.** A diff against `python-numpy` only restores
  on top of `python-numpy` — not on top of `python` or a forked
  variant. The base-tag pin is recorded in the manifest and enforced
  at restore time.
- **More than ~10 levels of chain depth.** Restore-time cost grows
  linearly with depth (one diff merge per level). We'll target tight
  performance for 1-3 levels; anything past that should `forkd
  snapshot compact` (collapse a chain into a fresh base — see "Open
  questions").
- **Network deltas (e.g. zsync, rsync over chain merges).** Hub
  transfers send the full diff file. Inside-LAN sync optimizations
  belong in a separate piece of work.

## Mechanism

### Storage layout

A chain-derived snapshot directory looks like:

```
~/.local/share/forkd/snapshots/python-numpy+pandas/
  snapshot.json    # { parent_tag: "python-numpy", memory: "diff.bin", ... }
  diff.bin         # sparse FC Diff file vs. parent's memory.bin
  vmstate          # full vmstate (vmstate doesn't chain — see "Risks")
  rootfs.ext4      # full rootfs (no FS chaining in v0.5; see Non-goals)
```

vs. a base snapshot:

```
~/.local/share/forkd/snapshots/python-numpy/
  snapshot.json    # { parent_tag: null, memory: "memory.bin", ... }
  memory.bin       # full guest RAM
  vmstate
  rootfs.ext4
```

The shape is uniform: `snapshot.json` declares whether `memory` is a
full file or a diff against `parent_tag`. `parent_tag = null`
distinguishes bases.

### Chain resolution at restore

The controller's existing `Snapshot::load` resolves a tag to a
`(memory_path, vmstate_path, rootfs_path)` triple. We extend it to
walk parents:

```
resolve_chain(tag):
  chain = [tag]
  while snapshot.json[chain[-1]].parent_tag is not None:
    chain.append(parent_tag)
  chain.reverse()           # [base, +pandas, +sklearn]
  return chain
```

For restore:

1. Resolve chain top-to-bottom.
2. Create a per-spawn scratch `memory.bin` (a `cp --reflink=auto` of
   the base; falls back to plain `cp` on non-CoW filesystems).
3. For each subsequent link in the chain, overlay its `diff.bin`
   onto the scratch memory file. This is the same merge logic as
   v0.3 diff snapshots
   ([`docs/design/diff-snapshots.md`](./docs/design/diff-snapshots.md)):
   walk the sparse file, copy non-hole pages at their offsets.
4. Use the **topmost** vmstate (vmstate doesn't chain; only the
   final state's vmstate is meaningful).
5. Hand the assembled `memory.bin` + topmost `vmstate` to FC's
   restore path.

Restore latency = `cp(base) + sum(merge(diff_i))`. For a 1.5 GiB base
+ 50 MB delta: 1.5 GiB cp (~6 s on HDD) + 50 MB merge (~0.4 s) =
~6.4 s. **Within 10% of base restore** (ROADMAP done-criterion 2)
because the merge cost is dominated by the cp.

(Reflink — when supported — collapses the cp to a metadata-only
operation, dropping restore latency to the base's restore time
exactly. Hub recipes should recommend btrfs or ext4-with-reflink for
this.)

### Build-time flow

`forkd snapshot diff --from <base> --tag <new> --exec <cmd>`:

```
1. Sanity: <base> exists, is registered, has memory.bin (or is
   itself the head of a chain that ultimately roots at a base).
2. Restore <base> into a one-shot sandbox (re-using `forkd fork`
   internals; `n=1`, throwaway netns).
3. Wait for the guest agent. Run `<cmd>` via /exec on the agent.
4. Pause the source.
5. FC `PUT /snapshot/create snapshot_type: "Diff"` —
   writes a sparse `diff.bin` containing only the pages dirtied
   since restore.
6. Capture the post-state vmstate.
7. Write `snapshot.json { parent_tag: <base>, memory: "diff.bin",
   ... }`.
8. Tear down the sandbox.
9. Register the new tag with the daemon (if running).
```

The cost of step 5 is the same as a v0.3 BRANCH diff write —
sub-second for typical workloads. Steps 2-4 cost the base's restore
time plus the install's wall time (the user-visible part of the
build, dominated by `pip install`).

Compared to status quo (full re-snapshot of base+pandas): build time
roughly the same (we still have to run the install), but disk drops
from "base + delta" to "delta" because the base bytes are already on
disk.

### CLI surface

**New verbs:**

```bash
# Build a diff tag by running a command on top of a base.
forkd snapshot diff --from python-numpy --tag python-numpy+pandas \
    --exec "pip install pandas==2.0.0"

# Inspect chain depth + cumulative bytes.
forkd snapshot info python-numpy+sklearn
# > base:        python-numpy
# > chain:       python-numpy → +pandas → +sklearn (3 levels)
# > diff bytes:  12 MB (this level), 24 MB (cumulative chain)
# > parent disk: 1.5 GiB

# Collapse a chain into a fresh base (see "Open questions").
forkd snapshot compact python-numpy+sklearn --tag python-numpy-flat
```

**Extended verbs:**

```bash
# Existing `forkd ls --snapshots` shows parent_tag column.
# Existing `forkd rmi <tag>` errors if other tags chain off it.
# Existing `forkd pack <tag>` walks the chain — includes parent bytes.
# Existing `forkd pull <tag>` understands chained manifests in registry.json.
```

### REST surface

**New endpoint:**

```
POST /v1/snapshots/diff
{
  "from": "python-numpy",
  "tag": "python-numpy+pandas",
  "exec": ["pip", "install", "pandas==2.0.0"],
  "exec_timeout_secs": 600
}
→ 201 SnapshotInfo { tag, parent_tag, dir, created_at_unix, ... }
```

**Existing endpoints extended:**

`SnapshotInfo` gains `parent_tag: Option<String>` (omitted /
`undefined` on base snapshots; SDK types updated correspondingly).

`POST /v1/sandboxes { snapshot_tag: "python-numpy+pandas" }` works
unchanged — the controller chases the chain at restore time, opaque
to the caller.

`DELETE /v1/snapshots/<tag>` errors with `409 Conflict` if any
registered tag has `parent_tag == <tag>`. Body lists the dependents.

### Hub integration

`registry.json` schema gains `parent_tag` in each recipe entry. The
existing pack/unpack path needs to walk:

- `forkd pack python-numpy+pandas`: includes pandas's `diff.bin` AND
  the parent's full bytes (transitive). Total pack size = sum of
  chain. Manifest declares the chain order.
- `forkd unpack`: writes each chain element to its own snapshot dir,
  preserves `parent_tag` in each `snapshot.json`.
- `forkd pull deeplethe/python-numpy+pandas`: the registry entry
  records the chain; pull fetches each link. Each link's hash is
  verified independently against the manifest.

The "include parent bytes" cost is unfortunate but unavoidable
without a content-addressable storage layer (out of scope). A future
v0.6 OCI-style layered Hub could deduplicate the base across
multiple `+delta` tags on the server side.

## Alternatives considered

### A. Full re-snapshot (status quo)

`forkd snapshot --tag python-numpy+pandas` against a `+pandas`
docker image. Works today; that's what we ship. Cost: re-pull base
image, re-build rootfs, re-boot, re-warm, re-snapshot. Several GB
of duplicated bytes per delta.

**Rejected** because the iteration loop is the bottleneck this
milestone explicitly targets.

### B. Filesystem-level CoW (overlayfs / btrfs reflink)

Use the host kernel's CoW primitives to derive `+pandas/rootfs.ext4`
from `python-numpy/rootfs.ext4` and `+pandas/memory.bin` from
`python-numpy/memory.bin`. Modify one, the kernel transparently
shares unchanged pages.

**Rejected for memory.bin**: FC writes `memory.bin` once at snapshot
time. After that it's a static file. CoW doesn't help with the
*creation* path — you'd still have to materialize the full delta on
write. It does help with disk usage (vs. status quo), but
`snapshot_type: "Diff"` is strictly better because it stores only
dirty pages, not "all pages including unchanged ones referenced
via CoW."

**Partially relevant for rootfs**: a future piece of work could
layer overlayfs over rootfs.ext4 to chain filesystem deltas. Out of
scope for v0.5; see Non-goals.

### C. OCI-style layered images

Store snapshots as content-addressable layers, each a tarball of
changed pages. Pull = fetch the layers you don't have, assemble
locally.

**Deferred to v0.6+**. The mechanism we ship in v0.5 is forwards-
compatible: a `parent_tag`-style chain is the simplest
content-addressable model. Moving to true CAS is a Hub-side change
(`registry.json` format + storage backend) that doesn't require
rebuilding the on-disk client format.

### D. Just diff the memory and re-derive the rootfs from a base image

`+pandas` would store only the memory diff; rootfs gets regenerated
from `python-numpy:base + pip install pandas` on each restore.

**Rejected**: regenerating rootfs is slow (the original problem) AND
non-deterministic (pip can't reproduce the exact same wheels months
later). We need the rootfs as-recorded.

## Open questions

### 1. Maximum chain depth — what to enforce?

Restore cost grows linearly. A 10-level chain on a 1.5 GiB base is
~6 s + 10 × 0.4 s = ~10 s restore. Beyond that, `forkd snapshot
compact` should be the user's escape valve.

Proposal: warn at depth 5, error at depth 10. Override via
`--allow-deep-chain` flag.

### 2. Compacting a chain

`forkd snapshot compact <chain-head> --tag <new-flat-tag>`:
restore the chain, immediately snapshot it as a fresh base (one
full memory.bin), register under the new tag. The chain is left
intact; user can later `forkd rmi` the original head if they want.

Question: should compact happen automatically when the chain
crosses depth N? Probably not — invisible work that consumes GB of
disk is unfriendly. Keep it manual.

### 3. vmstate compatibility across chain links

Each link's vmstate is captured against a slightly different RAM
layout (because the install changed kernel page tables, allocated
new file-backed mmaps, etc.). The restore path uses the
**topmost** vmstate against the assembled memory. This works
because the assembled memory at restore time matches the memory at
the topmost snapshot's capture time — both are the result of
running the same chain of operations.

**Risk**: if the install at level N changes something the kernel
serializes into vmstate but doesn't persist in memory (e.g.
in-kernel state tied to a host file path that's gone at restore
time), restore will fail. Need to scope an empirical test in
Phase 1 against a representative set of installers (`apt install`,
`pip install`, `npm install`).

### 4. What if a parent is updated after deriving a diff?

User does `forkd snapshot --tag python-numpy:v2 ...` to rev the
base, but `python-numpy+pandas` still references `python-numpy`
(v1). Two policies:

- **Pinning by content hash** (preferred): `parent_tag` resolves
  not by name but by `(name, sha256-of-base-memory.bin)`. If the
  base changes content, the diff explicitly fails to restore.
- **Pinning by name**: name resolves to current; user is
  responsible for not rev-ing bases under derived tags.

The former is safer; the latter is simpler. v0.5 ships the latter
with a content-hash field recorded for diagnostic purposes; v0.6
upgrades to enforce.

## Implementation phases

### Phase 1 — `snapshot.json` schema + restore-side resolver (~3 days)

- Add `parent_tag: Option<String>` to `SnapshotInfo` (api.rs) and
  `snapshot.json` schema.
- Implement `resolve_chain(tag)` in `forkd-controller`.
- Extend `Snapshot::load` to accept a chain and assemble memory.bin
  via `cp(base) + merge(diff_i)` for each link.
- Hand-craft a 2-level chain on disk to exercise the resolver
  without the build verb yet.
- 5 unit tests + 1 integration test (assembled restore round-trips
  bytes for a known input).

### Phase 2 — `forkd snapshot diff` CLI / REST (~5 days)

- New REST endpoint `POST /v1/snapshots/diff`.
- New CLI verb `forkd snapshot diff --from --tag --exec`.
- Reuse the v0.3 `Snapshot::create_diff` machinery; only the bind
  point changes (build-time vs. branch-time).
- Daemon wiring: stand up a one-shot sandbox from the base, run
  exec, snapshot diff, register the new tag.
- 3 integration tests including a `pip install` happy path.

### Phase 3 — chain-aware Hub (`pack`, `unpack`, `pull`, registry) (~4 days)

- `forkd pack` walks the chain, manifest declares chain order +
  per-link hashes.
- `forkd unpack` writes each link into its own snapshot dir,
  preserving `parent_tag`.
- `registry.json` schema updated; pull fetches each link.
- 2 integration tests: pack-unpack round-trip on a 3-level chain;
  pull from a fixture HTTP server.

### Phase 4 — `forkd snapshot info` / `compact` / `rmi` interaction (~2 days)

- `forkd snapshot info` shows chain depth, cumulative bytes,
  parent.
- `forkd snapshot compact` materializes a fresh base from a chain
  head.
- `forkd rmi` blocks on dependents with the actionable error.

### Phase 5 — bench + writeup (~3 days)

- Build `python-numpy`, derive `+pandas`, derive `+pandas+sklearn`.
- Measure: each link's diff size, restore time vs base.
- Verify the two done-criteria (diff < 100 MB, restore within 10%).
- `bench/diff-snapshot-chains/RESULTS-v0.5.md`.

### Phase 6 — docs (~1 day)

- Update README + README-zh with chain example.
- Update `docs/HUB.md` for chained recipes.
- Update `docs/API.md` for the new endpoint + extended SnapshotInfo.
- CHANGELOG entry.

**Total: ~3 weeks.** Aligns with ROADMAP M2.1's estimate.

## Risks

### vmstate drift across chain links

The biggest unknown. If a Phase 2 test shows that `pip install`'s
post-state vmstate doesn't restore cleanly against the assembled
memory (because of in-kernel state we didn't anticipate), we may
need to either:

- Pre-flight check in `forkd snapshot diff`: after the diff is
  taken, restore it as a sanity check, fail loudly if the restore
  errors.
- Constrain the install commands we support to a known-good set
  (apt, pip, npm) where empirical testing covers the failure
  modes.
- Fall back to "diff is memory-only, restore is regenerative" for
  some workloads (loses the deterministic-restore property).

Budget +1 week to Phase 2 if this hits — same line item as the
v0.3 ext4 mballoc compound (which was also a "real-kernel
behavior didn't match the design") risk we cleared in v0.3.4 with
posix_fallocate.

### Disk usage on Hub-pull of a deep chain

A 5-level chain published to the Hub means a `forkd pull` downloads
the full base (1.5 GiB) plus 4 diffs (~50 MB each). User who only
wants the head sees 1.7 GiB of bytes for a "small" pull. We can
hand-wave that as "diff chains save the recipe maintainer's time,
not the recipe consumer's" — true but worth surfacing in the
docs.

A real fix is CAS-style layered Hub (alternative C); v0.5 ships
the naive scheme and documents the cost.

### `forkd rmi <base>` accidentally breaking chains

Mitigated by the 409-with-dependents-list approach. User has to
explicitly `rmi` each dependent (or pass `--cascade` if we add
that — open question).

## References

- v0.3 BRANCH-side diff design: [`docs/design/diff-snapshots.md`](./docs/design/diff-snapshots.md).
- Firecracker `snapshot_type: "Diff"` mechanism:
  [firecracker-microvm/firecracker docs/snapshotting/snapshot-support.md](https://github.com/firecracker-microvm/firecracker/blob/main/docs/snapshotting/snapshot-support.md).
- ROADMAP M2.1 done-criteria: [`ROADMAP.md`](./ROADMAP.md).
- v0.4 live-fork (companion BRANCH-side optimization): [`DESIGN-v0.4.md`](./DESIGN-v0.4.md).
