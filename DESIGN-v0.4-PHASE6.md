# v0.4 Phase 6 — `mode="live"` BRANCH path

**Status:** DRAFT — implementation blueprint.
**Predecessor:** Phases 5a/5b/5c (memfd + MAP_SHARED) — done as of 2026-05-29 (see [`docs/VENDORED-FIRECRACKER.md`](./docs/VENDORED-FIRECRACKER.md)).
**Successor:** Phase 7 (REST/CLI/SDK surface), Phase 8 (doctor checks), Phase 9 (benchmarks) — see [`DESIGN-v0.4-USER-API.md`](./DESIGN-v0.4-USER-API.md).
**Tracking issue:** [#101](https://github.com/deeplethe/forkd/issues/101).
**Estimated effort:** ~2.5 weeks (5 incremental PRs after the 2026-05-29 scope correction below).

## What changes

Today `branch_sandbox` in `crates/forkd-controller/src/http.rs` handles `--full` and `--diff` paths inline — both call `vm.snapshot_to` / `vm.snapshot_diff_to` which trigger Firecracker's synchronous `Snapshot.Create`. The source VM is paused for the full duration of `memory.bin` being written to disk (~150 ms for a 1 GiB parent on ext4).

Phase 6 adds a third path, `mode="live"`, which:

1. Acquires a `WpBranch` (in [`crates/forkd-uffd/src/wp_snapshot.rs`](./crates/forkd-uffd/src/wp_snapshot.rs), already shipped) over the source's memfd region.
2. Pauses Firecracker.
3. Issues a **`SnapshotType::VmstateOnly`** snapshot — dumps vmstate JSON, writes nothing to `memory.bin`.
4. Arms `UFFDIO_WRITEPROTECT` across the whole guest RAM region via `WpBranch::begin`.
5. Resumes Firecracker. Source pause window now contains only steps 2-4: target **< 10 ms**.
6. Drives `WpBranch::bulk_copy_clean` in a background task, which streams clean pages from the memfd into the destination `memory.bin` while the WP handler thread captures any guest writes.
7. On completion (or `wait: false` returning early), `WpBranch::finalize` returns the dirty-fault count and arming/copy timings.

The result is a `memory.bin` byte-identical to what `--full` would have produced, but the source VM observed a < 10 ms blackout instead of ~150 ms.

## The FC blocker — `SnapshotType::VmstateOnly`

Firecracker's `PUT /snapshot/create` requires `mem_file_path` and always writes guest memory to it (full region for `Full`, dirty pages for `Diff`). There is no variant that dumps vmstate without touching memory. See [`DESIGN-v0.4-PHASE3-SPIKE.md`](./DESIGN-v0.4-PHASE3-SPIKE.md) for the prior analysis.

**Decision:** add a fourth commit to the vendored FC branch ([`deeplethe/firecracker:forkd-v0.4-mem-backend-shared-v1.12`](https://github.com/deeplethe/firecracker/tree/forkd-v0.4-mem-backend-shared-v1.12)) introducing `SnapshotType::VmstateOnly`. Same vendor strategy that Phases 5a/5b already use; consistent with the Option B + parallel Option A outcome of the Phase 3 spike.

Patch shape (~30 LOC across 2-3 files):

- `src/vmm/src/persist.rs::create_snapshot` — early-return after the vmstate dump when `snapshot_type == VmstateOnly`.
- `src/vmm/src/vmm_config/snapshot.rs::SnapshotType` — new variant; `Default` stays `Full`.
- `src/firecracker/src/api_server/request/snapshot.rs` — accept the new value in `mem_file_path: Optional` form (since vmstate-only doesn't need a backing file).

Forkd-side: `Vm::snapshot_vmstate_only(vmstate: PathBuf) -> Result<()>` in `crates/forkd-vmm/src/lib.rs`, issuing `{"snapshot_type": "VmstateOnly"}`.

Rejected alternatives:

- **Tee approach (FC writes memory to `/tmp/throwaway`, forkd writes the real `memory.bin` via `WpBranch`).** Doubles disk I/O on every live BRANCH. The whole point of v0.4 is reducing I/O cost; can't burn another 150 ms of writes per BRANCH for the privilege of being on stock FC.
- **Pass FC `/dev/null` as `mem_file_path`.** `set_len` rejects character devices (FC's `snapshot_memory_to_file` calls `file.set_len(expected_size)`); confirmed in DESIGN-v0.4-PHASE3-SPIKE.md.
- **Bypass FC's snapshot API entirely (forkd serializes vmstate by reading KVM ioctls directly).** FC owns the VM fd; getting it out requires either `ptrace` or FC-side cooperation. High cost, high risk of vmstate-format drift breaking restore compatibility. Not worth it for the ~30 LOC saved.

## Snapshot file format

`memory.bin` written by `mode="live"` is byte-identical to what `--full` produces — same dense layout, no sparse holes, no separate header. The bulk copier writes pages contiguously from offset 0; the WP handler writes dirty pages at their byte offset using `pwrite`. Because the WP-arming guarantees every page is captured exactly once across the two flows, the final file has no gaps.

This means `mode="live"` snapshots are restore-compatible with stock Firecracker (just like `--full` and `--diff` outputs are today). No `format_version` bump in `vmstate` JSON; restore code in `crates/forkd-vmm/src/lib.rs::restore_many_with` is untouched.

USER-API doc's open question #5 ("snapshots written by `--live` not backward-compatible") is resolved as: **they are compatible.** The doc's concern came from a sparse-file design that we've now ruled out; updating the doc is part of PR 6.4 below.

## Where `UFFDIO_REGISTER` must happen — a scope correction

(Discovered during Phase 6.2 implementation, 2026-05-29.)

`userfaultfd(2)` is per-process: `UFFDIO_REGISTER` can only register VMAs *in the same process that called `userfaultfd()`*. The fd can be passed elsewhere via `SCM_RIGHTS`, but only the creator can register additional VMAs.

KVM runs inside Firecracker's process; guest writes go through FC's EPT/VMA. A `UFFDIO_WRITEPROTECT` armed by forkd-controller against its *own* mmap of the memfd would only trap controller-process writes — guest writes via `KVM_RUN` in FC's process would silently bypass it. The Phase 2 PoC (`experiments/v0.4-kvm-uffd-wp-poc/RESULTS.md`) verified UFFD_WP catches KVM writes only because that PoC ran KVM and the uffd handler in the **same** process.

The original plan in this doc — "WpBranch::begin in controller arms WP on FC's memory" — therefore does not work. Correct mechanism, modeled on FC's existing restore-side UFFD support (`backend_type: "Uffd"`):

1. FC creates the userfaultfd inside its own process.
2. FC issues `UFFDIO_REGISTER` (WP mode) against its own guest-memory VMA.
3. FC sends the fd to forkd-controller via `SCM_RIGHTS` over a UDS path the controller provided.
4. forkd-controller, holding the received fd, can:
   - issue `UFFDIO_WRITEPROTECT` to arm WP across the region (events still fire because the registration FC did covers the whole region),
   - poll the fd for fault events,
   - read pages directly from its own MAP_SHARED mmap of the memfd (the bulk-copy path is unchanged — it's just a read against memory the controller already has).

This adds **one more incremental PR** in front of the original 6.2:

| PR | Scope |
|---|---|
| **6.1.5** | Add `POST /uffd/wp` endpoint to vendored FC. Body: `{"socket": "<UDS path>"}`. FC creates uffd, registers WP, connects to socket, sends fd via `SCM_RIGHTS`. ~50 LOC + tests. Pattern-matches `guest_memory_from_uffd` in `src/vmm/src/persist.rs`. |
| **6.2 (revised)** | Controller side: `Vm::request_wp_uffd(socket_path)` — listens on the socket, issues the new FC endpoint, receives fd via `recvmsg + SCM_RIGHTS`. Existing receiver code in `crates/forkd-uffd/src/lib.rs` is the pattern. Also expose `Vm::memfd_handle()` so the bulk-copy mmap can be set up in the controller's process. |

PRs 6.3 and 6.4 are unchanged in shape; 6.3's `WpBranch::begin` will take an *externally-registered* uffd (skip the register step internally).

Estimate moves from ~2 weeks to ~2.5 weeks. The bigger picture stands: snapshot file format is still byte-identical to `--full`, the live BRANCH still consists of (WP-arm) + (vmstate dump) + (resume) + (async copy), and the API surface (`mode="live"`, `wait`) is unchanged. Only the fd-acquisition path needed to be made honest.

## Integration point in `branch_sandbox`

Today's `branch_sandbox` (http.rs:574) has a `match req.diff` shape. After Phase 6 it becomes:

```rust
let mode = req.resolve_mode()?;  // Phase 7: REST plumbing; Phase 6 uses internal enum
match mode {
    BranchMode::Full => { /* existing full path */ }
    BranchMode::Diff => { /* existing diff path */ }
    BranchMode::Live { wait } => {
        // 1. Get memfd handle from vm (added in PR 6.2).
        let memfd = vm.memfd_handle().context("--live requires memfd-backed sandbox")?;

        // 2. Pre-allocate destination memory.bin (same as --full).
        preallocate_memory_file(&dst_mem, source_size)?;

        // 3. WpBranch::begin — registers uffd, arms WP. Sub-ms per GiB.
        let wp = WpBranch::begin(memfd.try_clone()?, region_addr, region_size, dst_mem.clone())?;

        // 4. Vmstate-only snapshot. Writes JSON; does NOT touch memory.bin.
        let pause_start = Instant::now();
        vm.pause()?;
        vm.snapshot_vmstate_only(snap_dir_for_task.join("vmstate"))?;
        let resume_result = vm.resume();
        pause_ms = Some(pause_start.elapsed().as_millis() as u64);

        // 5. Drive bulk copy. WP handler thread already running inside WpBranch.
        let copy_handle = std::thread::spawn(move || {
            wp.bulk_copy_clean()?;
            wp.finalize()
        });

        if wait {
            let stats = copy_handle.join().map_err(...)??;
            // Stats include wp_arm_ms, async_copy_ms, dirty_pages_caught.
        } else {
            // Phase 6.4: stash copy_handle in shared state keyed by tag,
            // return immediately with status="writing".
        }
    }
}
```

Two pieces of plumbing needed before this shape compiles:

1. **`Vm::memfd_handle()` getter.** Phase 5b already stored `pub memfd: Option<memfd::MemfdRegion>` on `Vm`. Expose a `try_clone()`-able borrow. Also surface the guest region's `(addr, size)` so `WpBranch::begin` knows what to WP.
2. **`Vm::snapshot_vmstate_only(vmstate: PathBuf)`** wrapping the new FC API call.

## Status tracking (for `wait: false`)

When `wait: false`, `branch_sandbox` returns once the source resumes, but the destination `memory.bin` is still being written. State to track:

- The `JoinHandle` of the bulk-copy thread.
- The tag → "writing" / "ready" / "failed" mapping.

**Decision:** in-memory only, per USER-API decision. Persist to `registry.json` only on transition to `ready` (or `failed` on daemon restart mid-write — those snapshots get marked `failed` and require re-BRANCH).

New shared state in `AppState`:

```rust
pub struct InFlightBranches {
    by_tag: parking_lot::Mutex<HashMap<String, BranchCopyHandle>>,
}

struct BranchCopyHandle {
    join: thread::JoinHandle<Result<WpBranchStats>>,
    started_at: Instant,
}
```

Endpoint to query: extends existing `GET /v1/images/<tag>` with `status: "writing"` until the join completes. Background reaper polls completed handles and promotes them to registry.

## PR breakdown

| PR | Scope | Done when |
|---|---|---|
| **6.1** | Add `SnapshotType::VmstateOnly` to vendored FC branch (`forkd-v0.4-mem-backend-shared-v1.12`); add `Vm::snapshot_vmstate_only` wrapper in `forkd-vmm`; smoke test on dev box that vmstate JSON is written and `memory.bin` is untouched. | Tree compiles, FC accepts the new field, integration test passes. |
| **6.2** | Expose `Vm::memfd_handle()` + region geometry getters. Update `Vm` to remember `(region_addr, region_size)` from `boot` and `restore_many_with`. | Existing tests pass; new test confirms `memfd_handle` is `Some` after memfd-backed boot, `None` after file-backed boot. |
| **6.3** | First-cut `mode="live"` path in `branch_sandbox`, sync-only (no `wait: false` support). Internal enum `BranchMode`; REST still accepts only `diff: bool` for now (Phase 7 wires the public surface). Smoke test: live BRANCH produces a `memory.bin` whose contents match a parallel `--full` BRANCH of the same parent (modulo pages dirtied between the two snapshots). | `--live` works end-to-end via test-only flag; live `pause_ms` < 50 ms on the dev box's coding-agent parent (target < 10 ms; 50 is the "obviously works" gate). |
| **6.4** | `wait: false` support: in-flight branch tracking, `GET /v1/images/<tag>` status field, background reaper. Update `DESIGN-v0.4-USER-API.md` open Q #4 (in-memory v0.4, persist v0.5+) and Q #5 (snapshot format IS backward-compatible). | `wait: false` returns within 10 ms of pause-exit; status flips to `ready` within `async_copy_ms`; daemon restart mid-write marks snapshot `failed`. |

Phases 7-9 then proceed independently — they consume the now-working `BranchMode::Live` enum value and don't change its internals.

## Open questions

1. **Concurrent `--live` BRANCHes on the same parent.** Two simultaneous WP-arming on the same uffd would race. The existing `try_acquire_branch_slot(&tag)` serializes by tag, but two different tags branching the same parent concurrently is currently allowed for `--diff`. **Plan:** for v0.4, gate `--live` BRANCHes through a per-parent mutex (added to `AppState`). Two parents can `--live` in parallel; one parent serializes its lives. Document the constraint; revisit when usage data shows it's a real bottleneck.

2. **What if `WpBranch::begin` fails after `vm.pause()`?** The source is paused with no WP armed. Need to `vm.resume()` before propagating the error. **Plan:** RAII guard around the pause in PR 6.3 (`PauseGuard` that calls `vm.resume()` on drop unless committed).

3. **Bulk-copy thread vs tokio runtime.** `WpBranch::bulk_copy_clean` is sync, takes the file lock, runs to completion. Spawning it on the controller's tokio runtime via `tokio::task::spawn_blocking` is fine, but the join-handle tracking in PR 6.4 needs to be `Send` and runtime-agnostic. **Plan:** stick with `std::thread::spawn` + a `Mutex<HashMap<tag, JoinHandle>>`; tokio talks to it via `spawn_blocking(move || handle.join())`.

4. **What does `forkd images <tag>` show during writing?** USER-API §3 promises `status: writing` and a size that grows. **Plan:** initially show the tag with `status: writing, size: -1`. PR 6.4's reaper updates size on transition to `ready`. Real-time size polling (stat the in-flight `memory.bin`) is a nice-to-have, deferred to Phase 9 benchmarks if it turns out to help diagnose stuck branches.

## Non-goals

- `--live` chained off `--live` (live BRANCH of a live-BRANCHed parent). The chain semantics get hairy because the parent's memfd is itself the output of a previous WP-copy. Deferred to v0.5.
- Per-parent throttling of bulk-copy bandwidth — if the host has 4 parents BRANCHing at once and one disk, they currently all hammer it. Mitigation deferred until benchmarks (Phase 9) show it matters.
- Migration of in-flight `mode="live"` BRANCHes across daemon restart. The daemon owns the memfd handle and the bulk-copy thread; both die with the daemon. Snapshots in flight become `failed`. Persisting in-flight branches across restart is at minimum a v0.5 problem.

## References

- [`DESIGN-v0.4.md`](./DESIGN-v0.4.md) — kernel mechanism (memfd + `UFFDIO_WRITEPROTECT` + async copier).
- [`DESIGN-v0.4-USER-API.md`](./DESIGN-v0.4-USER-API.md) — user-facing surface (`--live`, `mode="live"`, `wait`).
- [`DESIGN-v0.4-PHASE3-SPIKE.md`](./DESIGN-v0.4-PHASE3-SPIKE.md) — FC integration options (RESOLVED: vendor patch).
- [`docs/VENDORED-FIRECRACKER.md`](./docs/VENDORED-FIRECRACKER.md) — operational details on the FC fork.
- [`crates/forkd-uffd/src/wp_snapshot.rs`](./crates/forkd-uffd/src/wp_snapshot.rs) — the `WpBranch` library API (already on main).
- PoC results: `experiments/v0.4-*-poc/RESULTS.md`.
