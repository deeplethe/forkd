# v0.5: diff snapshot chains

**Status:** DRAFT — revised after first-pass self-review (see
[PR #212 comment](https://github.com/deeplethe/forkd/pull/212#issuecomment-4586836761)
for what changed).
**Target:** v0.5, 3 weeks if vmstate compat clean, 4 weeks if not
(per ROADMAP M2.1; honest buffer included).

**Done criteria** (lifted from ROADMAP.md, with explicit FS scope):

1. `forkd snapshot diff --from <base-tag> --tag <new-tag>` produces a
   diff `< 100 MB` for an `apt install` / `pip install` delta.
2. Restore time on a 3-snapshot chain is within 10% of base restore
   **on a reflink-capable filesystem** (btrfs, or ext4 with the
   reflink feature flag, or xfs). On non-reflink FS the milestone
   targets "within 2× of base" with `forkd doctor` warning the user.
   See [Storage shape](#storage-shape) for why.
3. Snapshot Hub MVP (M1.2) understands chains: pulling a diff also
   pulls its parents.

**Explicitly out of done-criteria** (may still ship as auxiliary
verbs if cheap, but not gating):

- `forkd snapshot compact` — collapse a chain into a fresh base.
  Listed in Phase 4 as an escape valve, but the milestone doesn't
  block on it.
- Live-fork interop. v0.5 ships file-backed chains; `live_fork:
  true` on a chained source is documented as v0.6 work (see
  [Live-fork interaction](#live-fork-interaction)).

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

## Prior art already in the tree

Most of the building blocks ship today. v0.5's job is wiring + a new
top-level verb, not a new primitive:

| Capability | Where | Status |
|---|---|---|
| `Vm::snapshot_diff_to(diff_path)` — emit a sparse Diff snapshot | `crates/forkd-vmm/src/lib.rs:1088` | ✅ shipped (v0.3.1) |
| `apply_diff(diff, base)` — merge sparse diff onto a base file | `crates/forkd-vmm/src/lib.rs:1553` | ✅ shipped (v0.3.1), 3 unit tests |
| Chain semantics: "previous BRANCH output is next diff's base" | `SandboxInfo.last_branch_memory_path` (`forkd-controller/src/state.rs`) + phase 1d in [`docs/design/diff-snapshots.md`](./docs/design/diff-snapshots.md) | ✅ shipped (v0.3.1) |
| `track_dirty_pages: true` on FC machine-config | `forkd-vmm` default since v0.1.x | ✅ shipped |

**What this means for the risk story**: v0.3.1's BRANCH chain
already restores vmstate against a memory image assembled from a
base + N sparse diffs. The "vmstate against modified RAM"
compatibility question — flagged as the biggest unknown in the
first-pass draft — is **largely settled empirically** by v0.3.1's
production track record. The narrower risk for v0.5 is "vmstate of
an *install-time* state (pip / apt) drifts from the assembled
memory in ways v0.3.1's BRANCH-time states didn't" — possible but
specific, and Phase 2 plans an empirical test.

What's missing for M2.1:

- **A build-time `forkd snapshot diff` verb** (boot base, exec
  install, snapshot diff vs base).
- **Persisting the chain on disk** (snapshot.json `parent_tag` field).
  v0.3.1's chain lives in-process on `SandboxInfo`; chains across
  daemon restarts and across hosts need on-disk metadata.
- **Restore-time chain resolution** (walk `parent_tag`, assemble
  memory). v0.3.1 only resolves single-level "last_branch →
  prev_branch's memory.bin"; v0.5 needs N-level walks.
- **Hub support for chains** (`pack` / `pull` follow `parent_tag`).

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

### Storage shape

There are two viable shapes for chained snapshots on disk. Both
build on the same FC primitive:

| Shape | Each link stores | Restore cost | Disk savings |
|---|---|---|---|
| **A. Materialized** (v0.3.1 BRANCH chain) | Full `memory.bin` | One file open | None — every link is a full copy |
| **B. Delta** (proposed for v0.5) | Sparse `diff.bin` vs parent | `cp(base) + merge(diff)` per link | Linear with chain depth |

The first-pass draft tried to claim both A's restore cost and B's
disk savings — they don't compose. **v0.5 picks B (delta storage)**.
Reasoning:

- The whole point of the milestone is "stop duplicating 1.5 GiB
  base bytes per `pip install` delta." Picking A loses that.
- B's restore cost is `cp(base) + merge(diffs)`. On a
  reflink-capable FS the cp collapses to a metadata-only operation
  and we hit done-criterion 2 cleanly. On non-reflink FS we degrade
  to ~6 s cp (HDD) / ~1.5 s cp (SATA SSD), missing the "within
  10%" bar.
- We can recover non-reflink performance via **lazy
  materialization** — mmap the base + apply diffs at fault time
  via UFFD, reusing the v0.4 UFFD_WP machinery. Defer to v0.6 if
  v0.5's reflink+warning path is good enough; see
  [Alternative C](#c-lazy-materialization-via-uffd-v06-candidate).

Layout for a chain-derived (delta) snapshot:

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

Restore latency depends on the host FS:

| FS | `cp` cost (1.5 GiB) | Merge (50 MB diff) | Chain total | vs base |
|---|---|---|---|---|
| btrfs / xfs / ext4 with reflink | ~1 ms (metadata only) | ~0.4 s | ~0.4 s | **within 10% ✓** |
| ext4 no-reflink, SATA SSD | ~1.5 s | ~0.4 s | ~1.9 s | ~2× of base |
| ext4 no-reflink, HDD | ~6 s | ~0.4 s | ~6.4 s | ~6× of base |

**v0.5 ships the reflink path as the primary case** and adds a
`forkd doctor` check that flags non-reflink hosts with a clear hint
(\"M2.1 chains restore faster on btrfs / xfs / ext4-with-reflink; on
this host expect a ~2-6× restore slowdown — see
`bench/diff-snapshot-chains/RESULTS-v0.5.md` for numbers\"). Done-
criterion 2 is met on reflink FS; non-reflink targets "within 2× of
base" — relaxed but documented.

Long-term recovery for non-reflink hosts is lazy materialization via
UFFD (see [Alternative C](#c-lazy-materialization-via-uffd-v06-candidate));
deferred to v0.6 so v0.5 lands on a finite scope.

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

### Live-fork interaction

Two questions, both with carve-outs for v0.5:

**1. Can `forkd snapshot diff` use a `live_fork: true` source?**
   No, v0.5. The build-time path boots the base in a one-shot
   sandbox; v0.5 hard-codes `live_fork: false` for that sandbox
   (file-backed RAM). Reason: a memfd-backed source needs the
   vendored FC fork to be installed on the build host, which adds
   a host requirement for what should be a pure CLI verb. File-
   backed source restore is universally available.

   This means *building* a chain works on any host; only *live
   BRANCH from the chain at use time* would require the vendored
   FC, and that's already understood from v0.4.

**2. Can a chained snapshot be spawned with `live_fork: true`?**
   Yes, eventually — but v0.5 ships without it. The chain
   resolver assembles a `memory.bin` via `cp(base) + merge(diff)`
   into a file. Setting `live_fork: true` on `POST /v1/sandboxes
   { snapshot_tag: "python-numpy+pandas" }` would need that
   assembled file to feed into the memfd-populate path
   (`memfd::create_and_populate` in `forkd-vmm/src/memfd.rs`).

   The memfd path *should* compose cleanly because it just reads
   bytes from a file — the controller doesn't care that the file
   was assembled from a chain. Two known unknowns:

   - The `mem_backend.backend_path: "<chained-assembled-path>"`
     restore body needs to point at the assembled file, not
     `python-numpy+pandas/diff.bin`.
   - The assembled file needs to live somewhere persistent enough
     that the FC process can mmap it for the VM's lifetime — and
     get cleaned up when the sandbox dies.

   v0.5 declares this combination as **explicitly unsupported**
   (returns HTTP 400 from `POST /v1/sandboxes` if both
   `snapshot_tag` is chained and `live_fork: true` are set). v0.6
   wires it; the file-management story is a separate small design
   doc.

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

### C. Lazy materialization via UFFD (v0.6 candidate)

Instead of `cp(base) + merge(diff)` upfront, mmap the base
`memory.bin` read-only and register a UFFD handler. At fault time
the handler checks the chain's diffs (top-down) for the faulting
page and serves the topmost match, falling back to the base mmap.
Pages are materialized lazily as the guest touches them.

This is the read-time analogue of the v0.4 UFFD_WP machinery
(`crates/forkd-uffd/`). Same handler shape, opposite direction:
v0.4 captures writes, v0.6 would serve reads. Could reuse the
`accept_handshake` and SCM_RIGHTS plumbing.

**Why deferred, not in v0.5:**

- **Restore correctness is harder to prove.** Eager
  `cp + merge` produces a single file with known contents; the
  guest can't observe any difference between that and a base
  snapshot. Lazy serves bytes on demand — any bug in the diff-
  walking handler shows up as guest corruption.
- **Performance only helps non-reflink hosts.** Reflink hosts
  already pay metadata cost for the `cp`; lazy doesn't help them.
- **It's another ~2 weeks of work on top of M2.1**, including
  another empirical compat test against vmstate (this time
  vmstate-against-lazily-materialized-RAM).

**v0.5 ships eager + reflink + doctor warning**; lazy lands in
v0.6 if non-reflink users complain or if the Hub use case
(downloading deep chains over slow networks) makes it worth the
complexity.

### D. OCI-style layered images

Store snapshots as content-addressable layers, each a tarball of
changed pages. Pull = fetch the layers you don't have, assemble
locally.

**Deferred to v0.6+**. The mechanism we ship in v0.5 is forwards-
compatible: a `parent_tag`-style chain is the simplest
content-addressable model. Moving to true CAS is a Hub-side change
(`registry.json` format + storage backend) that doesn't require
rebuilding the on-disk client format.

### E. Just diff the memory and re-derive the rootfs from a base image

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

### 3. vmstate compatibility — narrower risk than first pass claimed

The first pass flagged "vmstate against modified RAM" as the biggest
unknown. v0.3.1's BRANCH chain (which restores against base + N
sparse diffs) is empirical evidence that the general shape works.

The narrower v0.5-specific risk: **install-time** state may serialize
things v0.3.1's BRANCH-time states don't. Specifically:

- File-backed mmaps with paths that exist at install time but not
  at restore time (`/tmp/pip-installer-xyz/`). Mitigation: the
  restore environment matches the install environment closely
  (same FC, same kernel, same rootfs path layout).
- Kernel timers / kvmclock skew across the install pause. Same
  mitigation as v0.3.1 — FC's vmstate already accounts for this.
- Network state from `pip install`'s HTTPS connection. Should be
  closed before pause; we explicitly wait for `exec` to exit
  before calling `snapshot/create`.

**Phase 2 empirical test**: pick three installers (`pip install
pandas`, `apt install jq`, `npm install lodash`), build a chain
for each, restore + run a smoke command in the chained snapshot,
verify the install is present and functional. If all three pass
unmodified, the risk closes. If one fails, scope the fix to that
class of installer (likely a pre-pause cleanup pass).

### 4. Parent pinning — content-hash, not name (committed)

User does `forkd snapshot --tag python-numpy:v2 ...` to rev the
base, but `python-numpy+pandas` still references `python-numpy`.
Two options:

- **Pinning by name**: name resolves to current. Simple but
  user can silently break their chain by re-snapshotting the
  base.
- **Pinning by content hash**: `parent_tag` resolves by
  `(name, sha256-of-base-memory.bin)`. Re-snapshotting the base
  causes restore to fail loudly.

**v0.5 ships content-hash pinning as the default.** First-pass
deferred this to v0.6, but a silent foot-gun isn't acceptable for
the v0.5 GA. Implementation: `snapshot.json` for a chained tag
includes `parent_content_hash: "<sha256>"`. Restore verifies the
parent's current `memory.bin` against the recorded hash; on
mismatch, the controller returns HTTP 409 with the actionable
message ("chain `<tag>` references `<parent>` content
`<hash-prefix>...`, but parent now has content `<other-hash>`;
rebuild with `forkd snapshot diff --from <parent>`").

## Implementation phases

### Phase 1 — `snapshot.json` schema + restore-side resolver (~3 days)

- Add `parent_tag: Option<String>` and `parent_content_hash:
  Option<String>` to `SnapshotInfo` (api.rs) and the on-disk
  `snapshot.json` schema. Both default `None` for bases; both
  required for chained snapshots.
- Implement `resolve_chain(tag) -> Vec<SnapshotInfo>` in
  `forkd-controller`, walking parents top-down to root.
- Extend `Snapshot::load` to accept a chain and assemble
  `memory.bin` via `cp(base) + apply_diff(diff_i)` per link.
- Detect reflink support at runtime and prefer
  `cp --reflink=auto`; fall back to plain `cp` with a one-time
  log warning.
- Hand-craft a 2-level chain on disk to exercise the resolver
  without the build verb yet.

Unit tests (in `forkd-controller`):

1. `resolve_chain` on a base returns `[base]`.
2. `resolve_chain` on a depth-3 chain returns the three links in
   `[base, +pandas, +sklearn]` order.
3. `resolve_chain` errors with actionable message if any parent
   is missing.
4. `resolve_chain` errors if cycle detected (parent_tag chain
   loops back to self).
5. `parent_content_hash` mismatch produces HTTP 409 with the
   parent name + recorded vs current hash.
6. Reflink available → restore latency stays within 10% of base
   (measured against a `tmpfs`-mounted snapshot root in CI).
7. Reflink unavailable → restore still produces byte-identical
   assembled memory (verified by hash of the resulting file).

Integration test (in `forkd-controller/tests/`):

- Hand-craft a 2-level chain on disk, restore via `POST
  /v1/sandboxes { snapshot_tag: "<chain-head>" }`, exec a probe
  command in the spawned sandbox, verify the probe sees both the
  base's state and the chained delta. Smoke-quality, not a
  perf gate.

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

**Total**:

- **3 weeks** if the Phase 2 vmstate empirical test passes against
  pip / apt / npm unchanged. Phases 3 (Hub) and 4 (info/compact/rmi)
  can run in parallel — they touch disjoint files (`hub.rs` vs.
  controller registry / CLI). That parallelism shaves ~2 days off the
  serial estimate, so the realistic serial path is 3.5 weeks before
  parallelism.
- **4 weeks** if Phase 2 hits the vmstate-class issue called out
  in the Risks section, requiring an extra pre-pause cleanup pass or
  per-installer carve-outs.

Aligns with ROADMAP M2.1's estimate with buffer made explicit.

## Risks

### vmstate drift on install-time states

Narrower than the first-pass draft claimed: v0.3.1's BRANCH chain
already restores vmstate against assembled memory in production
(see [Prior art](#prior-art-already-in-the-tree)). The
v0.5-specific risk is install-time state classes v0.3.1 doesn't
exercise — pip wheel-extraction `mmap`s, apt's dpkg lockfiles,
npm's `node_modules` build temporaries.

If a Phase 2 empirical test (pip / apt / npm chain restore + smoke
exec) hits a failure, the fix scopes to that class:

- Pre-pause cleanup pass in the build flow (`forkd snapshot diff`
  shells out a final `sync` / `rm -rf /tmp/install-*` before
  `snapshot/create`).
- Per-installer carve-outs documented in `docs/HUB.md`.
- In the worst case, the diff verb errors with a clear message on
  the affected installers and the milestone ships with a known
  carve-out list rather than blocking on a perfect fix.

Budget: **+1 week to Phase 2** if any installer fails the smoke
test. Same shape as the v0.3.4 ext4 mballoc fix (~1 week of
diagnose + posix_fallocate landing).

### Disk usage on Hub-pull of a deep chain

A 5-level chain published to the Hub means a `forkd pull` downloads
the full base (1.5 GiB) plus 4 diffs (~50 MB each). User who only
wants the head sees 1.7 GiB of bytes for a "small" pull.

Honest framing for the docs: "diff chains save the recipe
maintainer's iteration time and on-disk redundancy. They don't
save the recipe consumer's bandwidth — for that, the v0.6 CAS-
layered Hub deduplicates the base across multiple `+delta` tags
on the server side."

v0.5 ships the naive scheme and documents the cost rather than
hiding it.

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
