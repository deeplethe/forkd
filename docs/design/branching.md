# Sandbox branching

**Status:** shipped in v0.2 (PR #49 controller / #50 CLI / #51 SDK).
**REST:** `POST /v1/sandboxes/:id/branch` on the controller daemon.
**CLI:** `forkd snapshot --from-sandbox <id> [--tag <tag>]`.
**Python SDK:** `Controller.branch_sandbox(sandbox_id, tag=None)`.

## Why

forkd today supports `parent snapshot → N children` (a two-layer tree,
fan-out terminates at fork). Modal-style **branching** turns this into
an arbitrary-depth tree where any running child can itself be snapshot'd
and forked into grandchildren.

Concrete agent-runtime use cases this unlocks:

- **Safe destructive ops** — branch before `rm -rf` / `apt remove`;
  discard on regret, commit on confirm.
- **A/B tool execution** — agent tries `grep` vs `ripgrep` in parallel
  branches, picks the winner.
- **Conversation checkpointing** — auto-branch every N turns in
  production; if turn 47 misbehaves, attach to turn 45's snapshot to
  reproduce.
- **SWE-bench style iteration** — branch after expensive setup
  (clone + install), iterate on the test prompt without re-doing setup.

Without branching, today's agent developers either avoid destructive
ops, manually restore state, or restart conversations from scratch.

## What ships in v0.2

The mechanism = `vm.pause() → vm.snapshot_to() → vm.resume()` against a
running child VM. **All three primitives already exist in
`crates/forkd-vmm/src/lib.rs`** (`pause` at L716, `snapshot_to` at L724,
`resume` needs verification — add if missing). The work is wiring,
not new kernel-level engineering.

### Surface

**REST** (controller daemon, requires bearer token):

```
POST /v1/sandboxes/:id/branch
  body: { "tag": "<optional-branch-tag>" }
  → 201 { tag, dir, vmstate, memory, created_at_unix, branched_from }
```

Behaviour:
1. Look up sandbox `:id` in `live_vms`. 404 if not present.
2. Call `vm.pause()` (source sandbox now stops vCPUs).
3. Call `vm.snapshot_to(<snapshot_root>/<tag>/...)`.
4. Write `snapshot.json` (mirrors `create_snapshot()` flow).
5. Call `vm.resume()` (source sandbox runs again).
6. `registry.insert_snapshot(...)` with `branched_from: Some(sandbox_id)`.
7. Return `SnapshotInfo`.

**CLI:**

```bash
# Phase 1 (this milestone)
forkd snapshot --from-sandbox sb-67a1b3-0042 --tag spec-1
forkd fork --tag spec-1 -n 5

# Phase 2 (follow-up, optional)
forkd branch --from-sandbox sb-67a1b3-0042 -n 5     # one-shot helper
```

**Python SDK:**

```python
with Sandbox(tag="pyagent") as sb:
    sb.commands.run("apt install -y something-risky")
    branch = sb.branch()              # POST /v1/sandboxes/{sb.id}/branch
                                       # → SnapshotInfo (the new tag)
    grandchildren = branch.fork(n=5)   # spawns from the new snapshot
    for g in grandchildren:
        g.commands.run("...")
```

## Design decisions (committed)

1. **Tag default:** `branch-<source_sandbox_id>-<seq>` where `seq` is
   per-source-sandbox monotonic. Readable, debuggable.

2. **No ephemeral branches in v0.2.** Every branch persists to the
   snapshot registry until explicitly deleted. Adds ~30% complexity to
   support transient branches; not worth it before users ask for it.

3. **No depth limit.** Document the disk cost (each branch level
   carries a full `memory.bin`) and stop there. If a user wants 10
   branches of a 2 GiB-memory parent, they consciously commit 20 GiB.

4. **Volume mounts inherit.** `--volume` settings recorded in
   `snapshot.json` are propagated unchanged from source to branch. Zero
   extra work; the snapshot serialization already covers volumes.

5. **`branched_from` field on `SnapshotInfo`:** `Option<String>` carrying
   the source sandbox id. Used for audit / debugging / lineage queries.
   Does **not** create a hard dependency — branches remain valid after
   the source sandbox is killed.

6. **Source-sandbox lifecycle independence.** Branching does not change
   source sandbox state visible to the user. The source may be killed
   immediately after a branch returns; the branch survives (it's a
   snapshot on disk).

## Pause-window semantics

Between `pause()` and `resume()`, the source sandbox is frozen at the
vCPU level:

- TCP sockets stay open (kernel keeps them).
- TCP application-level keepalive *may* time out depending on remote
  side and configuration.
- Sleeping processes don't notice the pause; on resume they continue
  as if no time passed.
- Outside observers (other VMs trying to talk to it) see a timeout.

**Pause window duration:** dominated by `vm.snapshot_to()` —
~0.5–8 s for memory.bin sizes 256 MiB to 8 GiB on local NVMe. For
LLM-inference parents with ≥4 GiB resident, plan for ~3–5 s.

This trade-off is acceptable and explicit in docs. Modal's branch has
the same semantics.

## Non-goals (this milestone)

- **Live branching (no pause):** would require userfaultfd-based
  copy-on-write of the source VM's memory; significant new engineering.
  Defer to v0.3+.
- **Branch chain merge / promote.** No `merge_from()` API. v0.2 users
  manually copy filesystem deltas back if they want; SDK adds a helper
  later if real demand emerges.
- **Cross-host branching.** Branches live on the host that made them.
  Multi-host distribution waits for snapshot diff/incremental work.
- **Branch GC policies.** All branches persist until explicit delete.
  Auto-prune by age / count is a v0.3 nice-to-have.

## Failure modes

| Failure | Behaviour |
|---|---|
| Source sandbox not in `live_vms` | 404, no side effect |
| `vm.pause()` fails | 500, source sandbox state unchanged (firecracker still running) |
| `vm.snapshot_to()` fails | 500, attempt `vm.resume()` to recover source; partial snapshot files cleaned |
| `vm.resume()` fails after successful snapshot | 500, source sandbox is dead (firecracker may still be paused); snapshot file persists and is valid |
| Disk full during snapshot_to | 500 with disk-full error; source VM resumed |
| Tag collision | 409 Conflict; no side effect |

The trickiest case is "snapshot succeeded but resume failed." This
leaves source as effectively dead while the branch is intact. Probably
acceptable: the snapshot itself is the user's escape hatch. Document
in failure-modes section of API doc.

## Implementation plan

### Phase 1 — REST end-to-end (this PR, ~half-day)

1. **`crates/forkd-vmm/src/lib.rs`** — confirm `vm.resume()` exists; add
   if not. ~5 LoC.
2. **`crates/forkd-controller/src/http.rs`** — add `POST
   /v1/sandboxes/:id/branch` handler. Mirror `create_snapshot()`
   structure. ~60 LoC.
3. **`crates/forkd-controller/src/api.rs`** — extend `SnapshotInfo`
   with `branched_from: Option<String>`. Update existing serializers.
4. **`crates/forkd-controller/src/state.rs`** — `insert_snapshot()`
   accepts the new field. Persistence already JSON, no schema migration.
5. **dev-box smoke test** — bring up postgres-fixture, fork one child,
   branch it, fork 3 grandchildren, verify each has independent psql.

### Phase 2 — CLI (next PR, ~half-day)

6. `crates/forkd-cli/src/main.rs` — add `--from-sandbox <id>` flag to
   `snapshot` subcommand. Calls the controller REST endpoint.
7. `docs/API.md` — document the new endpoint + behaviour.
8. `docs/RUNBOOK.md` — operator notes on pause-window semantics.

### Phase 3 — SDK + demo (next PR, ~1 day)

9. `sdk/python/forkd/sandbox.py` — add `Sandbox.branch() -> Snapshot` +
   `Snapshot.fork(n=...) -> list[Sandbox]`.
10. `recipes/postgres-fixture/demo.py` — extend with a branching example.
11. Top-level `README.md` — short subsection under "Quick start"
    advertising branching.

### Phase 4 — promote design doc

12. Move `notes/design/branching.md` → `docs/design/branching.md` as
    part of the final PR (or the first PR if we're confident).

## Test plan

- **Unit:** mock `Vm` for snapshot/pause/resume; assert handler calls
  in the right order, restores on failure.
- **Integration (dev-box):** the smoke test above + multi-branch
  isolation (branch source A and B from same parent; A and B don't
  influence each other).
- **Failure injection:** kill snapshot mid-flight via `sudo kill -9` of
  firecracker; assert handler returns 5xx and source VM state.
