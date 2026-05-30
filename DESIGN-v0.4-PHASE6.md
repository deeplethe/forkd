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

Patch shape (~30 LOC across 5 files — `SnapshotType` is matched exhaustively at four sites, so adding a variant requires a touch in each):

- `src/vmm/src/vmm_config/snapshot.rs::SnapshotType` — new variant; `Default` stays `Full`.
- `src/vmm/src/persist.rs::create_snapshot` — guard `snapshot_memory_to_file` so the memory dump is skipped when `snapshot_type == VmstateOnly`. `mem_file_path` stays `PathBuf` (required); the caller passes a placeholder that FC never opens.
- `src/vmm/src/rpc_interface.rs` — metric branch for the new variant.
- `src/vmm/src/vstate/vm.rs::snapshot_memory_to_file` — `SnapshotType::VmstateOnly => unreachable!()` in the inner match; the guard in `create_snapshot` should keep this from firing.
- `src/firecracker/src/api_server/mod.rs` — second metric branch (the API server has its own match site, separate from the rpc_interface one).

Serde deserialization picks up the new variant automatically; no change needed to `api_server/request/snapshot.rs`.

Forkd-side: `Vm::snapshot_vmstate_only(vmstate: PathBuf) -> Result<()>` in `crates/forkd-vmm/src/lib.rs`, issuing `{"snapshot_type": "VmstateOnly", "snapshot_path": ..., "mem_file_path": "/tmp/forkd-vmstate-only-mem-ignored"}` (placeholder path; FC accepts the field but never opens it for this snapshot type).

Rejected alternatives:

- **Tee approach (FC writes memory to `/tmp/throwaway`, forkd writes the real `memory.bin` via `WpBranch`).** Doubles disk I/O on every live BRANCH. The whole point of v0.4 is reducing I/O cost; can't burn another 150 ms of writes per BRANCH for the privilege of being on stock FC.
- **Pass FC `/dev/null` as `mem_file_path`.** `set_len` rejects character devices (FC's `snapshot_memory_to_file` calls `file.set_len(expected_size)`); confirmed in DESIGN-v0.4-PHASE3-SPIKE.md.
- **Bypass FC's snapshot API entirely (forkd serializes vmstate by reading KVM ioctls directly).** FC owns the VM fd; getting it out requires either `ptrace` or FC-side cooperation. High cost, high risk of vmstate-format drift breaking restore compatibility. Not worth it for the ~30 LOC saved.

## Snapshot file format

`memory.bin` written by `mode="live"` is byte-identical to what `--full` produces — same dense layout, no sparse holes, no separate header. The bulk copier writes pages contiguously from offset 0; the WP handler writes dirty pages at their byte offset using `pwrite`. Because the WP-arming guarantees every page is captured exactly once across the two flows, the final file has no gaps.

This means `mode="live"` snapshots are restore-compatible with stock Firecracker (just like `--full` and `--diff` outputs are today). No `format_version` bump in `vmstate` JSON; restore code in `crates/forkd-vmm/src/lib.rs::restore_many_with` is untouched.

USER-API doc's `§ Backward compatibility` section currently states "a snapshot file produced by v0.4 `--live` is *not* backward-compatible" and reserves a `format_version` bump for it. That claim is now resolved as: **they are compatible.** The original concern came from a sparse-file design that we've ruled out; updating that section (not the open-questions list — Q5 there is a different topic about Python SDK wait=False semantics) is part of PR 6.4 below.

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
| **6.1.5** | (DONE — FC commit [`7d80afade`](https://github.com/deeplethe/firecracker/commit/7d80afade)) Add `PUT /uffd/wp` endpoint to the vendored FC. Body: `{"socket": "<UDS path>"}`. FC creates uffd, registers WP across guest memory, connects to the caller's UDS, sends fd + region descriptors via `SCM_RIGHTS`. **Final shape:** 96 LOC across 8 files (mostly mechanical exhaustive-match touches), `userfaultfd = "0.8.1"` needed the `linux5_7` feature flag. **Two surprises documented for the historical record:** (a) FC's vmm-thread seccomp filter does not allow `userfaultfd(2)` (syscall 323) or the `UFFDIO_API` / `UFFDIO_REGISTER` / `UFFDIO_WRITEPROTECT` ioctls; calling them post-boot triggers SIGSYS. Workaround for development is `--no-seccomp`; productionizing the endpoint will require seccomp filter entries (tracked as a separate follow-up). (b) `UFFDIO_REGISTER (WP)` returns `EINVAL` on file-backed VMAs — the kernel correctly refuses to WP an ext4 mapping (`vma_can_userfault` allows only anon, tmpfs, shmem, and memfd). Our smoke test initially passed FC an ext4 path as `backend_path` and hit this; the real Phase 6 caller routes through `MemoryBackend::MemfdShared` (Phase 5b), so the VMA is shmem-backed and registration succeeds. Smoke test that proves the round-trip end-to-end against a memfd backend ships in [`scripts/dev/test-wp-uffd-memfd.py`](./scripts/dev/test-wp-uffd-memfd.py). |
| **6.2 (revised)** | Controller side: `Vm::request_wp_uffd(socket_path)` — listens on the socket, issues the new FC endpoint, receives fd via `recvmsg + SCM_RIGHTS`. Existing receiver code in `crates/forkd-uffd/src/lib.rs` is the pattern. Also expose `Vm::memfd_handle()` so the bulk-copy mmap can be set up in the controller's process. |

PRs 6.3 and 6.4 are unchanged in shape; 6.3's `WpBranch::begin` will take an *externally-registered* uffd (skip the register step internally).

Estimate moves from ~2 weeks to ~2.5 weeks. The bigger picture stands: snapshot file format is still byte-identical to `--full`, the live BRANCH still consists of (WP-arm) + (vmstate dump) + (resume) + (async copy), and the API surface (`mode="live"`, `wait`) is unchanged. Only the fd-acquisition path needed to be made honest.

## Integration point in `branch_sandbox`

Today's `branch_sandbox` (http.rs:574) has a `match req.diff` shape. After Phase 6 it becomes:

(Sketch updated for the scope correction above — `WpBranch::begin` no longer creates the uffd; FC does, and the controller receives it via SCM_RIGHTS.)

```rust
let mode = req.resolve_mode()?;  // Phase 7: REST plumbing; Phase 6 uses internal enum
match mode {
    BranchMode::Full => { /* existing full path */ }
    BranchMode::Diff => { /* existing diff path */ }
    BranchMode::Live { wait } => {
        // 1. Sanity: --live requires memfd-backed sandbox so the bulk copier
        //    has the same memory the guest sees (Phase 5b stored the memfd
        //    on Vm; getter added in PR 6.2).
        let memfd = vm.memfd_handle().context("--live requires memfd-backed sandbox")?;

        // 2. Pre-allocate destination memory.bin (same as --full).
        preallocate_memory_file(&dst_mem, source_size)?;

        // 3. Ask FC to set up the snapshot-side WP uffd in its own process
        //    and ship the fd back via SCM_RIGHTS over a UDS we listen on.
        //    PR 6.2 adds Vm::request_wp_uffd; PR 6.1.5 adds the FC endpoint.
        let wp_sock = workdir.join("wp.sock");
        let wp_uffd: OwnedFd = vm.request_wp_uffd(&wp_sock)?;
        //                ^^^^^^^^ already registered in WP mode by FC, against
        //                FC's guest_memory VMA. WRITEPROTECT calls + event polling
        //                still work from this process; UFFDIO_REGISTER would not.

        // 4. Start WpBranch with an externally-registered uffd. begin() now
        //    arms WP (UFFDIO_WRITEPROTECT) and spawns the handler thread;
        //    it does NOT call UFFDIO_REGISTER.
        let wp = unsafe {
            WpBranch::begin_with_external_uffd(
                wp_uffd,
                memfd.try_clone()?,    // for bulk_copy_clean's mmap read path
                region_addr,
                region_size,
                dst_mem.clone(),
            )?
        };

        // 5. Vmstate-only snapshot under a PauseGuard (see open Q below) so
        //    a failure between pause and resume cannot leave the source paused.
        let _pause = PauseGuard::pause(&vm)?;
        let pause_start = Instant::now();
        vm.snapshot_vmstate_only(snap_dir_for_task.join("vmstate"))?;
        drop(_pause);  // explicit resume; could be implicit on scope exit
        pause_ms = Some(pause_start.elapsed().as_millis() as u64);

        // 6. Drive bulk copy on a std::thread (the WpBranch handler thread
        //    is already running internally; it captures dirty pages while
        //    bulk copies the clean ones).
        let copy_handle = std::thread::spawn(move || -> Result<WpBranchStats> {
            unsafe { wp.bulk_copy_clean()?; }
            wp.finalize()
        });

        if wait {
            let stats = copy_handle.join().map_err(|e| anyhow!("copy thread panicked: {e:?}"))??;
            // stats: wp_arm_ms, async_copy_ms, dirty_pages_caught.
        } else {
            // PR 6.4: stash copy_handle in InFlightBranches keyed by tag,
            // return immediately with status="writing".
        }
    }
}
```

Three pieces of plumbing needed before this shape compiles:

1. **FC endpoint (PR 6.1.5):** `PUT /uffd/wp` on the vendored FC binary — creates the uffd, registers WP, sends fd via SCM_RIGHTS.
2. **`Vm::request_wp_uffd(socket_path: &Path) -> Result<OwnedFd>` + `Vm::memfd_handle()` getter (PR 6.2):** controller-side glue. Phase 5b already stored `pub memfd: Option<memfd::MemfdRegion>` on `Vm`; `memfd_handle()` exposes a `try_clone()`-able borrow plus the guest region's `(addr, size)` so PR 6.3 knows what to WRITEPROTECT.
3. **`Vm::snapshot_vmstate_only(vmstate: PathBuf)` (PR 6.1, done)** and **`WpBranch::begin_with_external_uffd(...)` (PR 6.3)** — the latter is a new constructor that takes an already-registered uffd and skips `UFFDIO_REGISTER`; the existing `WpBranch::begin` stays for the PoC.

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
| **6.1.5** | Add `PUT /uffd/wp` endpoint on the vendored FC branch (see the scope-correction section above for the constraints — seccomp allowlist additions for `userfaultfd(2)` + UFFDIO ioctls, plus EINVAL diagnosis vs the bare-process C probe). FC creates uffd, registers WP against guest_memory, connects to the caller's UDS, sends fd via SCM_RIGHTS. | `PUT /uffd/wp` returns 204; UDS receiver gets a valid fd; the received fd reports WP-relevant `ioctls` flags; FC survives the call (no seccomp SIGSYS) and the source VM stays alive afterwards. |
| **6.2** | Controller-side wiring. `Vm::memfd_handle()` + region geometry getters (Phase 5b stored the memfd on `Vm`; expose it). `Vm::request_wp_uffd(socket_path: &Path) -> Result<OwnedFd>` — listens on the UDS, issues `PUT /uffd/wp`, receives fd via `recvmsg + SCM_RIGHTS` (pattern from `crates/forkd-uffd/src/lib.rs`'s existing receiver). | Existing tests pass; new test confirms `memfd_handle` is `Some` after memfd-backed boot and `None` after file-backed boot; new test confirms `request_wp_uffd` against the patched FC returns an fd that can `UFFDIO_WRITEPROTECT` + `read()` an event. |
| **6.3** | First-cut `mode="live"` path in `branch_sandbox`, sync-only (no `wait: false` support). Internal enum `BranchMode`; REST still accepts only `diff: bool` for now (Phase 7 wires the public surface). Add `WpBranch::begin_with_external_uffd(...)` constructor that takes an already-registered uffd (the existing `begin` stays for the PoC + standalone use). `PauseGuard` RAII so a failure between pause and resume cannot leave the source paused. Smoke test: live BRANCH produces a `memory.bin` whose contents match a parallel `--full` BRANCH of the same parent (modulo pages dirtied between the two snapshots). | `--live` works end-to-end via test-only flag; live `pause_ms` < 50 ms on the dev box's coding-agent parent (target < 10 ms; 50 is the "obviously works" gate). |
| **6.4** | `wait: false` support: in-flight branch tracking, `GET /v1/images/<tag>` status field, background reaper. Update `DESIGN-v0.4-USER-API.md` — open Q #4 (in-memory v0.4, persist v0.5+) and the `§ Backward compatibility` section's claim that `--live` snapshots are not backward-compatible (they are; see "Snapshot file format" above for why). | `wait: false` returns within 10 ms of pause-exit; status flips to `ready` within `async_copy_ms`; daemon restart mid-write marks snapshot `failed`. |

Phases 7-9 then proceed independently — they consume the now-working `BranchMode::Live` enum value and don't change its internals.

## Open questions

1. **Concurrent `--live` BRANCHes on the same parent.** Two simultaneous WP-arming on the same uffd would race. The existing `try_acquire_branch_slot(&tag)` serializes by tag, but two different tags branching the same parent concurrently is currently allowed for `--diff`. **Plan:** for v0.4, gate `--live` BRANCHes through a per-parent mutex (added to `AppState`). Two parents can `--live` in parallel; one parent serializes its lives. Document the constraint; revisit when usage data shows it's a real bottleneck.

2. **What if `snapshot_vmstate_only` fails between `vm.pause()` and `vm.resume()`?** The source is paused with no path to resume. (Note: post-scope-correction, `WpBranch::begin_with_external_uffd` and the FC `/uffd/wp` setup happen BEFORE pause, so they cannot leave the source paused. The vulnerable region is now narrower: just the vmstate-only call between pause and resume.) **Plan:** RAII `PauseGuard` (see PR 6.3 in the breakdown above) calls `vm.resume()` on drop unless explicitly committed. The guard is dropped right after the vmstate-only call returns successfully.

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
