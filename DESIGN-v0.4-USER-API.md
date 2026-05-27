# v0.4 user-facing API — `--live` branch mode

**Status:** DRAFT — sketching the surface that production callers will see once the v0.4 live-fork path lands. Companion to:

- [`DESIGN-v0.4.md`](./DESIGN-v0.4.md) — the kernel-level mechanism (memfd + `uffd_wp` + async dirty-page copier).
- [`DESIGN-v0.4-USE-CASES.md`](./DESIGN-v0.4-USE-CASES.md) — the product cases unlocked by sub-50 ms BRANCH.
- [`DESIGN-v0.4-PHASE3-SPIKE.md`](./DESIGN-v0.4-PHASE3-SPIKE.md) — Firecracker integration options.

This document focuses on **what the surface looks like** once the implementation is done — what CLI flags, REST fields, and SDK methods callers actually use. The kernel mechanics are settled (4 PoCs passed: see `experiments/v0.4-*-poc/RESULTS.md`); the open question is how to expose them to callers in a way that doesn't break v0.3.4 users and is small enough to land incrementally.

**Tracking issue:** [#101](https://github.com/deeplethe/getdeeplethe/forkd/issues/101).

## Summary of the user-visible change

Today (v0.3.4):

```bash
sudo forkd snapshot --from-sandbox sb-X --diff --tag branch-Y
# pause window: ~150 ms (disk write of memory.bin dominates)
# returns when snapshot file is fully persisted
```

After v0.4:

```bash
sudo forkd snapshot --from-sandbox sb-X --live --tag branch-Y
# pause window: ~3-10 ms (WP arm + vmstate dump; no memory write inline)
# returns when snapshot file is fully persisted (async copy completed)
```

The visible *caller-side* wall-time is similar in both flows (snapshot file is the same size, disk write the same speed). What changes is the **source VM's pause time** — for the source VM, `--live` makes the BRANCH transparent. This is the entire point of v0.4 for the use cases in [`DESIGN-v0.4-USE-CASES.md`](./DESIGN-v0.4-USE-CASES.md): the source agent doesn't stutter.

## Surface inventory

### 1. CLI — `forkd snapshot`

Add one flag:

| Flag | Behavior |
|---|---|
| `--full` *(default)* | Same as today. Pause + full memory dump inline. |
| `--diff` | Same as today. Pause + dirty-only memory dump inline. |
| `--live` *(new)* | Pause + WP-arm. Source resumes within ~3-10 ms. Snapshot file completes async; this command blocks until the file is fully persisted. |
| `--live --no-wait` *(new)* | Return as soon as the source resumes. Snapshot file completes in the background; query state via `forkd images <tag>` (will show `status: writing` until done). |

`--full` / `--diff` / `--live` are mutually exclusive. If two are passed, error. If none, default to `--full` (today's behavior, no change).

Example:

```bash
# Source unblocks in ~5 ms, caller waits another ~150 ms for disk write.
sudo forkd snapshot --from-sandbox sb-X --live --tag branch-Y

# Source unblocks in ~5 ms, caller also returns then.
sudo forkd snapshot --from-sandbox sb-X --live --no-wait --tag branch-Y

# Then later:
forkd images branch-Y
# tag: branch-Y  status: ready  size: 512 MiB  ...
```

### 2. REST — `POST /v1/sandboxes/<id>/branch`

Current request body:

```json
{"diff": true}
```

Proposed addition — introduce `mode` as the canonical field, treat `diff: true` as a legacy alias:

```json
{"mode": "live"}              // new
{"mode": "live", "wait": false} // new, "--no-wait" equivalent
{"mode": "diff"}              // canonical name for current --diff
{"mode": "full"}              // canonical name for current default
{"diff": true}                // still accepted; mapped to mode="diff"
```

Rules:
- If neither `mode` nor `diff` set → `mode="full"`.
- If both set → 400 BadRequest with message "use `mode` xor legacy `diff`".
- `wait` defaults to `true` (block until snapshot fully persisted).

Response stays the same shape as today; new fields documented in §3 Telemetry.

### 3. SDK — Python (`forkd`)

```python
from forkd import Controller

c = Controller()

# Live branch, default — blocks until snapshot ready.
snap = c.branch_sandbox("sb-X", mode="live", tag="branch-Y")
print(snap.pause_ms)    # ~5
print(snap.wall_ms)     # ~150 (the async copy time)

# Live branch, no wait — returns once source resumes.
snap = c.branch_sandbox("sb-X", mode="live", tag="branch-Y", wait=False)
print(snap.status)      # "writing"
# ...later
c.wait_for_snapshot("branch-Y")  # or just `c.list_snapshots()` and check status

# Legacy diff still works.
snap = c.branch_sandbox("sb-X", diff=True, tag="branch-Y")
```

Argument compatibility:

- `mode: "full" | "diff" | "live"` — new, preferred.
- `diff: bool` — deprecated but supported. If both `mode` and `diff` passed → `ValueError`.
- `wait: bool = True` — new, only meaningful with `mode="live"`.

### 4. SDK — TypeScript (`@deeplethe/forkd`)

```typescript
import { Controller } from "@deeplethe/forkd";

const c = new Controller();

const snap = await c.branchSandbox("sb-X", { mode: "live", tag: "branch-Y" });
console.log(snap.pauseMs, snap.wallMs);

// No-wait variant:
const snap2 = await c.branchSandbox("sb-X", {
  mode: "live", tag: "branch-Y", wait: false,
});
// snap2.status === "writing"

// Legacy:
const snap3 = await c.branchSandbox("sb-X", { diff: true, tag: "branch-Y" });
```

Same compatibility rules as Python.

## Telemetry — what callers see in the response

The snapshot result gains three optional fields, all only populated when `mode="live"`:

| Field | Meaning |
|---|---|
| `pause_ms` | What the source VM actually waited. With `mode="live"`: ~3-10 ms. With other modes: same as today. |
| `wp_arm_ms` | Sub-metric: time spent in `UFFDIO_WRITEPROTECT` across the guest RAM region. Linear in RAM size (~3 ms / GiB). |
| `async_copy_ms` | Wall time of the background dirty-page copier from WP-arm to "snapshot fully persisted". Roughly the same as today's `--diff` pause for the same parent. |
| `dirty_pages_caught` | Count of pages the WP handler captured (vs the bulk copier picking up clean pages). Useful for evaluating per-workload behavior. |

For `mode != "live"`, only `pause_ms` is reported (matches today's surface).

## Backward compatibility

- v0.3.4 callers that don't pass `mode` or `live` see *zero* behavior change.
- v0.3.4 callers passing `diff: true` keep working — internally rewritten to `mode="diff"`.
- A new sandbox spawned by v0.4 daemon has memfd-backed RAM by default (required for `--live`). This is a daemon-internal change; callers don't see it.
- A snapshot *file* produced by v0.3.4 is forward-compatible — v0.4 restore reads it the same way.
- A snapshot file produced by v0.4 `--live` is *not* backward-compatible — it relies on v0.4's bulk-copy + WP-handler interleave format. v0.3.4 daemons cannot restore it. We will encode this in the snapshot manifest's `format_version` and error helpfully if a v0.3.4 daemon encounters it.

## Configuration / tuning knobs

Hidden defaults that production operators may want to override (all env-var-only initially; no daemon config-file surface yet):

| Env | Default | Purpose |
|---|---|---|
| `FORKD_LIVE_BRANCH_BULK_WORKERS` | `4` | Threads driving the bulk-copier path. Linear scaling up to disk bandwidth. |
| `FORKD_LIVE_BRANCH_WP_HANDLER_QUEUE` | `1024` | In-flight WP-fault buffer. Large values smooth bursty dirty rates; small values bound peak memory. |
| `FORKD_MEMFD_BACKING` | `auto` | `auto` / `force` / `disable`. `auto` uses memfd on kernels with `uffd_wp` shmem support; disable to force file-backed for debugging. |

Don't expose these via CLI flags in v0.4 — they're operational knobs, not user controls. Promote to CLI later if real users surface a need.

## `forkd doctor` additions

Add two checks to the existing 14:

15. **`uffd_wp` on memfd available**: probe whether the running kernel supports `UFFDIO_WRITEPROTECT` on shmem/memfd VMAs (5.7+). Without this, `--live` errors at request time.
16. **memfd backing in use for current sandboxes**: report how many sandboxes were spawned with memfd vs file backing. A user trying `--live` on a pre-v0.4 sandbox should see a helpful "this parent was spawned with file-backed RAM, re-spawn with v0.4 daemon" message.

## Phase breakdown — incremental PR plan

| Phase | Scope | Outcome | Estimated effort |
|---|---|---|---|
| **5a** | `forkd-vmm`: memfd backing as an option in `MemBackend` (already patched in `0001-feat-mem-backend-shared-option-for-MAP-SHARED.patch`; needs upstream + integration) | New sandboxes can opt in to memfd | ~1 week |
| **5b** | `forkd-controller`: spawn-time path uses memfd by default on supported kernels; fallback to file-backed otherwise | All v0.4 sandboxes are memfd-backed transparently | ~3 days |
| **6** | `forkd-controller`: implement `mode="live"` path — WP-arm in pause, async copier, write `memory.bin` post-pause | `branch_sandbox` actually performs a live fork | ~2 weeks (depends on Phase 3 spike resolution — FC `VmstateOnly` snapshot type) |
| **7** | REST + CLI + Python SDK + TS SDK plumbing for `mode` / `--live` / `wait`, plus the legacy aliasing for `diff: true` | Surface is callable; old surface still works | ~3 days |
| **8** | `forkd doctor` checks 15-16; help-text additions; CHANGELOG entry | Doctor catches misconfiguration; users discover the feature | ~1 day |
| **9** | Benchmarks: `bench/live-fork-pause-window.md` reproducing the PoC numbers under the real controller path; chart for the README's headline | Marketable numbers + regression-detection harness | ~3 days |

Total estimate: **~5 weeks** of focused work, assuming Phase 6 doesn't hit unexpected Firecracker surprises.

## Open questions for review

1. **`--live` as default, eventually?** Once stable, do we flip the default from `--full` to `--live`? Pro: callers automatically get the pause improvement. Con: snapshot file format gains a new dependency invisible to users; restore on older daemons silently breaks. Lean **no** until v0.5 — keep explicit opt-in until at least one minor cycle has shipped.

2. **`mode: "live-async"` as a real value, vs `wait: false`?** Two ways to express "don't block on disk":
   - `{"mode": "live", "wait": false}` — flag-style.
   - `{"mode": "live-async"}` — enum-style.

   Lean **flag-style** (`wait: false`) — `wait` generalizes to `mode: "diff"` if anyone ever wants async diff, and it composes cleanly. The enum approach forces a Cartesian explosion if we add more modes.

3. **Failure mode if kernel doesn't support `uffd_wp` on memfd?**
   - Option A: error at request time with hint "your kernel is too old; need 5.7+".
   - Option B: silently fall back to `mode="diff"` and emit a telemetry counter.

   Lean **A**: silent fallback hides perf regressions from callers who paid attention. Loud failure forces them to either upgrade or accept the old surface explicitly.

4. **Per-snapshot status field in `forkd images` output.** When `--live --no-wait` is used, the snapshot is initially `status: writing`. Where does this status live (in `registry.json`? in a separate state file? in memory only?), and how do we keep it consistent across daemon restarts mid-write?

   Lean: in-memory only is fine for v0.4 — if the daemon crashes mid-async-copy, the snapshot is unusable and we mark it `status: failed` on daemon restart. Persisting writing-state across crashes is v0.5+.

5. **What about Python SDK's blocking semantics with `wait=False`?** The SDK call returns immediately, but if the user then does `c.spawn_sandboxes(tag="branch-Y", n=10)` while the snapshot is still writing, what happens?

   Need to decide: block-and-wait inside spawn, or fail-fast with "snapshot still writing"? Lean **block-and-wait with timeout** (default 30s) — it's the principle of least surprise; the explicit `wait=False` user opted out of waiting at *branch* time, not at *spawn* time.

## Non-goals for v0.4

Just to keep this scope contained — what's NOT in this surface:

- **Multi-host BRANCH** — deferred to v0.5.
- **BRANCH chaining** beyond 1 level — the v0.3.4 multi-BRANCH issue (#146) was fixed differently; not relitigated here.
- **Per-page lazy-restore on the child side** — children still `mmap(MAP_PRIVATE)` the snapshot file post-completion. Lazy restore is its own design.
- **Configuration via daemon config file** — env vars only in v0.4. Config-file surface deferred until a real user asks for it.
