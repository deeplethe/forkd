//! HTTP REST server for the forkd controller daemon.
//!
//! Routes (v1):
//!   GET    /healthz                       liveness probe; bypasses auth
//!   GET    /version                       build + API version
//!   GET    /metrics                       Prometheus text format
//!   GET    /v1/snapshots                  list registered snapshots
//!   POST   /v1/snapshots                  build snapshot from kernel + rootfs
//!   DELETE /v1/snapshots/:tag             remove registry entry + on-disk files
//!   GET    /v1/sandboxes                  list active sandboxes
//!   POST   /v1/sandboxes                  fork N children from a snapshot tag
//!   GET    /v1/sandboxes/:id              one sandbox's metadata
//!   DELETE /v1/sandboxes/:id              terminate a sandbox
//!   POST   /v1/sandboxes/:id/ping         alive-probe through the guest agent
//!   POST   /v1/sandboxes/:id/exec         spawn a subprocess in the sandbox
//!   POST   /v1/sandboxes/:id/eval         eval a Python expression in PID 1
//!   POST   /v1/sandboxes/:id/branch       pause + snapshot + resume; new tag
//!
//! Auth and audit logging are layered on top of this router in
//! `lib.rs::run_daemon`. Tests in this file exercise the bare router.
use anyhow::Context as _;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use parking_lot::Mutex;
use serde_json::json;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::api::{
    BranchSandboxRequest, CompactSnapshotRequest, CreateSandboxRequest, CreateSnapshotRequest,
    CreateWorkspaceRequest, DeleteSnapshotQuery, ErrorBody, EvalRequest, EvalResponse, ExecRequest,
    ExecResponse, SandboxInfo, SnapshotInfo, SnapshotInfoDetail, SuspendWorkspaceRequest,
    VersionResponse, WorkspaceInfo, WorkspaceStatus,
};
use crate::state::{read_proc_starttime, registration_boot_id, Registry};
use forkd_vmm::ClockSyncOutcome;

const API_VERSION: &str = "v1";
const BUILD_VERSION: &str = env!("CARGO_PKG_VERSION");

pub struct AppState {
    pub registry: Registry,
    /// Live child VMs, keyed by sandbox id. Drop here = kill the VM.
    /// Kept separate from the persistent registry because `forkd_vmm::Vm`
    /// owns OS resources (Child process, cgroup) and isn't serializable.
    pub live_vms: Mutex<HashMap<String, forkd_vmm::Vm>>,
    /// Root directory for tagged snapshots on disk.
    pub snapshot_root: PathBuf,
    /// Tags currently being snapshotted by an in-flight BRANCH. Prevents
    /// two concurrent `POST /branch` calls targeting the same tag from
    /// racing to clobber memory.bin. The on-disk vmstate-existence check
    /// alone is a TOCTOU — by the time both requests get past it, both
    /// may try to write.
    pub branch_in_flight: Mutex<HashSet<String>>,
    /// Global concurrent-BRANCH cap. A snapshot can write several GiB
    /// of memory.bin; without a cap, an attacker can fill the disk by
    /// firing many BRANCHes in parallel.
    pub branch_sem: Arc<Semaphore>,
    /// Owner lease for the single shared host tap (`forkd-tap0`),
    /// enforced by the controller (security review #281).
    ///
    /// `forkd-tap0` is a host tap that can be attached by ONE live
    /// firecracker at a time. A mutex only serialized the restore call —
    /// it did NOT prevent a second VM from grabbing the tap after the
    /// first VM was already live (the first VM still holds the tap fd
    /// for its whole lifetime, so the second spawn would hit "Resource
    /// busy" even after acquiring the mutex). This lease makes that
    /// invariant explicit: while a shared-tap VM is live, a second
    /// shared-tap request is rejected with 503. `per_child_netns=true`
    /// spawns do NOT touch this (each child netns has its own tap), so
    /// parallel per-netns sandboxes are unaffected.
    ///
    /// `parking_lot::Mutex<Option<String>>` where the value is the owning
    /// sandbox id. The claim is made INSIDE the `spawn_blocking` task
    /// (so a cancelled async handler cannot release it mid-restore) and
    /// committed in the async handler AFTER `live_vms.insert()` (so a
    /// cancelled handler between restore success and registration does
    /// not wedge the tap permanently). Released when the VM leaves
    /// `live_vms` (delete / suspend).
    pub shared_tap_owner: std::sync::Arc<parking_lot::Mutex<Option<String>>>,
    /// The configured maximum the `branch_sem` was constructed with.
    /// Tracked separately for `/metrics` (`forkd_branch_concurrency_cap`)
    /// because `Semaphore` doesn't expose its initial permit count.
    pub branch_concurrency_cap: usize,
    /// Atomic netns offset allocator (forkd-child-N). Shared by the
    /// sandbox and workspace spawn paths so concurrent spawns never
    /// pick overlapping offsets (security review #282).
    pub netns_alloc: std::sync::Arc<crate::netns::NetnsAllocator>,
    /// Monotonic count of pre-restore orphan scans that found firecracker
    /// processes not tracked by `live_vms`. Exposed via `/metrics` as
    /// `forkd_orphan_firecrackers_detected_total` so operators can alert
    /// on the tap/netns/fd-collision precursor instead of relying on logs.
    pub orphan_firecrackers_detected: std::sync::atomic::AtomicU64,
    /// Scratch directory used for prewarm throwaway snapshots when
    /// `CreateSandboxRequest::prewarm` is set. Mirror of
    /// `DaemonConfig::prewarm_scratch_dir`.
    pub prewarm_scratch_dir: PathBuf,
    /// Phase 6.4: in-flight `wait: false` live BRANCHes. Each entry's
    /// thread is producing `memory.bin` asynchronously after the
    /// source has already resumed. Reaped lazily on the next
    /// `GET /v1/snapshots` (promoted to `registry` as `Ready` /
    /// `Failed`), or when `delete_snapshot` is called on the tag.
    /// In-memory only; daemon restart loses tracking and the
    /// associated snapshot files are unrecoverable.
    #[cfg(target_os = "linux")]
    pub live_in_flight: Mutex<HashMap<String, LiveBranchHandle>>,
    /// Test-only owner of the scratch `TempDir` backing `snapshot_root`,
    /// the registry file and `prewarm_scratch_dir`. Tying the directory's
    /// lifetime to the state means dropping the last `Arc` reaps it; the
    /// unit tests used to `std::mem::forget` the `TempDir` instead, which
    /// orphaned one directory per construction on every `cargo test` run
    /// (issue #273). Declared last so it drops after `registry`.
    #[cfg(test)]
    pub _tempdir: Option<tempfile::TempDir>,
}

/// Phase 6.4: handle for a background bulk-copy thread driving the
/// post-pause work of a `wait: false` live BRANCH.
#[cfg(target_os = "linux")]
pub struct LiveBranchHandle {
    /// Snapshot metadata we'll persist (with status flipped to
    /// `Ready`) when `join` completes successfully.
    pub info: crate::api::SnapshotInfo,
    /// Owns the bulk-copy + finalize pipeline. Stats on success; an
    /// anyhow chain on failure.
    pub join: std::thread::JoinHandle<anyhow::Result<forkd_uffd::wp_snapshot::WpBranchStats>>,
}

/// Default number of concurrent BRANCH operations the daemon will admit.
/// Each BRANCH writes a full memory.bin (typically 256 MiB – 8 GiB),
/// so the cap bounds peak transient disk usage.
pub const DEFAULT_BRANCH_CONCURRENCY: usize = 4;

pub type SharedState = Arc<AppState>;

/// RAII guard for an in-flight BRANCH slot. Constructed via
/// [`AppState::try_acquire_branch_slot`]. Dropping the guard releases
/// the in_flight tag entry and the global semaphore permit, so all
/// failure paths in the handler get cleanup for free.
pub struct BranchSlot {
    tag: String,
    state: SharedState,
    _permit: OwnedSemaphorePermit,
}

impl std::fmt::Debug for BranchSlot {
    // AppState/Registry don't impl Debug; only print the tag (which is
    // what tests assert on) and skip the rest.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BranchSlot")
            .field("tag", &self.tag)
            .finish()
    }
}

impl Drop for BranchSlot {
    fn drop(&mut self) {
        self.state.branch_in_flight.lock().remove(&self.tag);
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum BranchSlotError {
    /// A BRANCH for this exact tag is already in flight. Caller should
    /// 409 Conflict and let the client retry once the existing one
    /// completes.
    AlreadyInFlight,
    /// Daemon is at its configured concurrent-BRANCH cap. Caller should
    /// 503 Service Unavailable.
    CapacityExceeded,
}

impl AppState {
    /// Try to register a BRANCH for `tag` in the in-flight set. Returns
    /// a guard whose Drop releases the registration; failure cases are
    /// 409 (same tag already being branched) or 503 (global cap hit).
    pub fn try_acquire_branch_slot(
        self: &Arc<Self>,
        tag: &str,
    ) -> Result<BranchSlot, BranchSlotError> {
        // Acquire the global permit first. If we acquired in_flight first
        // and then failed on the semaphore, we'd have to back out the
        // HashSet insert — possible but ugly.
        let permit = self
            .branch_sem
            .clone()
            .try_acquire_owned()
            .map_err(|_| BranchSlotError::CapacityExceeded)?;
        let mut in_flight = self.branch_in_flight.lock();
        if !in_flight.insert(tag.to_string()) {
            // Permit goes out of scope here — released.
            return Err(BranchSlotError::AlreadyInFlight);
        }
        Ok(BranchSlot {
            tag: tag.to_string(),
            state: self.clone(),
            _permit: permit,
        })
    }
}

pub fn router(state: SharedState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/version", get(version))
        .route("/metrics", get(metrics))
        .route("/v1/snapshots", get(list_snapshots).post(create_snapshot))
        .route("/v1/snapshots/:tag", delete(delete_snapshot))
        .route("/v1/snapshots/:tag/info", get(snapshot_info))
        .route("/v1/snapshots/:tag/compact", post(compact_snapshot))
        .route("/v1/sandboxes", get(list_sandboxes).post(create_sandbox))
        .route("/v1/sandboxes/:id", get(get_sandbox).delete(delete_sandbox))
        .route("/v1/sandboxes/:id/exec", post(exec_sandbox))
        .route("/v1/sandboxes/:id/eval", post(eval_sandbox))
        .route("/v1/sandboxes/:id/ping", post(ping_sandbox))
        .route("/v1/sandboxes/:id/branch", post(branch_sandbox))
        .route(
            "/v1/workspaces",
            get(list_workspaces).post(create_workspace),
        )
        .route(
            "/v1/workspaces/:name",
            get(get_workspace).delete(delete_workspace),
        )
        .route("/v1/workspaces/:name/suspend", post(suspend_workspace))
        .route("/v1/workspaces/:name/resume", post(resume_workspace))
        .with_state(state)
}

async fn healthz() -> impl IntoResponse {
    Json(json!({ "ok": true }))
}

async fn version() -> impl IntoResponse {
    Json(VersionResponse {
        version: BUILD_VERSION.to_string(),
        api: API_VERSION.to_string(),
    })
}

async fn metrics(State(s): State<SharedState>) -> impl IntoResponse {
    let (snap_count, sb_count) = s.registry.counts();
    let branches_in_flight = s.branch_in_flight.lock().len();
    let branch_cap = s.branch_concurrency_cap;
    let orphans = s
        .orphan_firecrackers_detected
        .load(std::sync::atomic::Ordering::Relaxed);
    // Prometheus text format. Keep names stable — exporters depend on them.
    let body = format!(
        "# HELP forkd_snapshots_total Number of snapshots known to the controller.\n\
         # TYPE forkd_snapshots_total gauge\n\
         forkd_snapshots_total {snap_count}\n\
         # HELP forkd_sandboxes_active Number of active sandboxes (child VMs).\n\
         # TYPE forkd_sandboxes_active gauge\n\
         forkd_sandboxes_active {sb_count}\n\
         # HELP forkd_branches_in_flight Number of BRANCH operations currently writing memory.bin.\n\
         # TYPE forkd_branches_in_flight gauge\n\
         forkd_branches_in_flight {branches_in_flight}\n\
         # HELP forkd_branch_concurrency_cap Configured maximum concurrent BRANCH operations.\n\
         # TYPE forkd_branch_concurrency_cap gauge\n\
         forkd_branch_concurrency_cap {branch_cap}\n\
         # HELP forkd_orphan_firecrackers_detected_total Cumulative firecracker orphans detected.\n\
         # TYPE forkd_orphan_firecrackers_detected_total counter\n\
         forkd_orphan_firecrackers_detected_total {orphans}\n\
         # HELP forkd_build_info Build version of the controller binary.\n\
         # TYPE forkd_build_info gauge\n\
         forkd_build_info{{version=\"{BUILD_VERSION}\"}} 1\n"
    );
    (
        StatusCode::OK,
        [("content-type", "text/plain; version=0.0.4")],
        body,
    )
}

async fn list_snapshots(State(s): State<SharedState>) -> impl IntoResponse {
    // Phase 6.4: reap any wait=false live BRANCHes whose background
    // bulk-copy thread has finished, then merge still-running ones
    // into the response as Writing entries. Keeps writers visible to
    // clients polling for `Ready` without a separate endpoint.
    #[cfg(target_os = "linux")]
    reap_finished_live_branches(&s);
    let mut by_tag: BTreeMap<String, SnapshotInfo> = BTreeMap::new();
    for info in s.registry.list_snapshots() {
        by_tag.insert(info.tag.clone(), info);
    }
    if let Ok(rd) = std::fs::read_dir(&s.snapshot_root) {
        for entry in rd.flatten() {
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            let Some(tag) = dir
                .file_name()
                .and_then(|n| n.to_str())
                .filter(|tag| is_safe_tag(tag))
                .map(str::to_string)
            else {
                continue;
            };
            by_tag
                .entry(tag.clone())
                .or_insert_with(|| snapshot_info_from_disk_dir(tag, dir));
        }
    }
    let mut snapshots: Vec<SnapshotInfo> = by_tag
        .into_values()
        .map(annotate_snapshot_bootability)
        .collect();
    #[cfg(target_os = "linux")]
    {
        let in_flight = s.live_in_flight.lock();
        for handle in in_flight.values() {
            let mut info = handle.info.clone();
            info.bootable = false;
            snapshots.push(info);
        }
    }
    Json(snapshots)
}

/// #301: scan for orphaned firecracker processes before a restore. An
/// orphan is a firecracker PID on the host that is NOT tracked by
/// `live_vms` — leftovers from a prior (crashed) run that may hold tap
/// devices, netns slots, or fd budget and doom a new restore to
/// socket/restore failures. Best-effort, read-only: it never kills.
/// The lock is held only long enough to snapshot the known-PID set;
/// the `/proc` scan + logging run after the lock is released so the
/// scan doesn't serialize concurrent sandbox operations.
fn warn_orphan_firecrackers(s: &SharedState, tag: &str) {
    // Collect known PIDs under a short-lived guard, then drop the lock
    // before the blocking `/proc` readdir + per-PID reads.
    let known: std::collections::HashSet<u32> = {
        let live = s.live_vms.lock();
        live.values().map(|vm| vm.pid()).collect()
    };
    let orphans = forkd_vmm::scan_firecracker_orphans(&known);
    if orphans.is_empty() {
        return;
    }
    s.orphan_firecrackers_detected
        .fetch_add(orphans.len() as u64, std::sync::atomic::Ordering::Relaxed);
    tracing::warn!(
        tag = %tag,
        count = orphans.len(),
        pids = ?orphans.iter().map(|o| o.pid).collect::<Vec<_>>(),
        "orphaned firecracker processes detected before restore; \
         these may hold tap/netns resources and cause spawn failures"
    );
    for o in &orphans {
        tracing::warn!(
            pid = o.pid,
            elapsed_secs = o.elapsed_secs,
            cmdline = %o.cmdline,
            "orphaned firecracker process"
        );
    }
}

/// Phase 6.4: lazy reaper. Walks `AppState::live_in_flight`, joins
/// any threads that finished, persists their `SnapshotInfo` into the
/// registry with `status = Ready` (success) or `Failed` (error or
/// panic), and removes the entry. Called from `list_snapshots` so the
/// transition is visible on the next client poll without a separate
/// background task.
#[cfg(target_os = "linux")]
fn reap_finished_live_branches(s: &SharedState) {
    // Take the lock briefly to find finished tags so we can drop the
    // lock before joining (join is fast for finished handles but we
    // hold no other locks during it, just for hygiene).
    let finished_tags: Vec<String> = {
        let in_flight = s.live_in_flight.lock();
        in_flight
            .iter()
            .filter(|(_, h)| h.join.is_finished())
            .map(|(t, _)| t.clone())
            .collect()
    };
    for tag in finished_tags {
        let Some(handle) = s.live_in_flight.lock().remove(&tag) else {
            continue;
        };
        let mut info = handle.info.clone();
        match handle.join.join() {
            Ok(Ok(_stats)) => {
                info.status = crate::api::SnapshotStatus::Ready;
                info.bootable = true;
                tracing::info!(
                    tag = %tag,
                    "live BRANCH (wait=false): bulk-copy complete, promoted to Ready",
                );
            }
            Ok(Err(e)) => {
                info.status = crate::api::SnapshotStatus::Failed;
                info.bootable = false;
                info.warning = Some(format!("background bulk-copy failed: {e:#}"));
                tracing::warn!(
                    tag = %tag,
                    error = %e,
                    "live BRANCH (wait=false): bulk-copy failed, marked Failed",
                );
            }
            Err(panic) => {
                info.status = crate::api::SnapshotStatus::Failed;
                info.bootable = false;
                info.warning = Some("background bulk-copy thread panicked".to_string());
                tracing::error!(
                    tag = %tag,
                    ?panic,
                    "live BRANCH (wait=false): bulk-copy thread panicked, marked Failed",
                );
            }
        }
        if let Err(e) = s.registry.insert_snapshot(info) {
            tracing::warn!(
                tag = %tag,
                error = %e,
                "reap_finished_live_branches: failed to persist completed/failed entry",
            );
        }
    }
}

async fn list_sandboxes(State(s): State<SharedState>) -> impl IntoResponse {
    Json(s.registry.list_sandboxes())
}

async fn get_sandbox(State(s): State<SharedState>, Path(id): Path<String>) -> Response {
    match s.registry.get_sandbox(&id) {
        Some(sb) => Json(sb).into_response(),
        None => not_found(&format!("sandbox {id}")),
    }
}

async fn create_snapshot(
    State(s): State<SharedState>,
    Json(req): Json<CreateSnapshotRequest>,
) -> Response {
    if !is_safe_tag(&req.tag) {
        return bad_request("tag must be 1-64 chars, ASCII alnum or dash/underscore");
    }
    let kernel = PathBuf::from(&req.kernel);
    let rootfs = PathBuf::from(&req.rootfs);
    if !kernel.exists() {
        return bad_request(&format!("kernel not found: {}", kernel.display()));
    }
    if !rootfs.exists() {
        return bad_request(&format!("rootfs not found: {}", rootfs.display()));
    }

    // Cap boot_wait_secs so a hostile caller can't tie up a daemon worker
    // for u64::MAX seconds. 60 s is well above the largest measured boot
    // time in our recipes (postgres-fixture warms up in ~10 s).
    if req.boot_wait_secs > 60 {
        return bad_request("boot_wait_secs must be ≤ 60");
    }

    let snap_dir = s.snapshot_root.join(&req.tag);
    if snap_dir.join("vmstate").exists() {
        return bad_request(&format!(
            "snapshot {} already exists; DELETE first",
            req.tag
        ));
    }
    let work_dir = std::env::temp_dir().join(format!("forkd-snapshot-{}", req.tag));

    let cfg = build_snapshot_boot_config(&kernel, &rootfs, &work_dir, req.rw, req.tap.as_deref());
    let boot_wait = std::time::Duration::from_secs(req.boot_wait_secs);
    let snap_dir_for_task = snap_dir.clone();

    let snapshot_result = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let mut vm = forkd_vmm::Vm::boot(&cfg)?;
        std::thread::sleep(boot_wait);
        vm.pause()?;
        std::fs::create_dir_all(&snap_dir_for_task)?;
        // Volumes via daemon snapshot API will land in a follow-up commit;
        // for now snapshots created through the daemon are volume-less.
        // Use the CLI's `forkd snapshot --volume` for tag-shared caches.
        let snap = vm.snapshot_to(
            snap_dir_for_task.join("vmstate"),
            snap_dir_for_task.join("memory.bin"),
            Vec::new(),
        )?;
        // Persist Snapshot metadata so subsequent forks read back the same
        // (possibly volume-bearing) snapshot description.
        let meta = serde_json::to_vec_pretty(&snap)?;
        std::fs::write(snap_dir_for_task.join("snapshot.json"), meta)?;
        vm.kill()?;
        Ok(())
    })
    .await;

    match snapshot_result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return server_error(&format!("snapshot: {e:#}")),
        Err(e) => return server_error(&format!("blocking task panicked: {e}")),
    }

    let info = SnapshotInfo {
        tag: req.tag.clone(),
        dir: snap_dir.display().to_string(),
        created_at_unix: unix_now(),
        branched_from: None,
        pause_ms: None,
        diff_ms: None,
        diff_physical_bytes: None,
        diff_logical_bytes: None,
        warning: None,
        status: crate::api::SnapshotStatus::Ready,
        bootable: true,
    };
    if let Err(e) = s.registry.insert_snapshot(info.clone()) {
        return server_error(&format!("persist snapshot: {e:#}"));
    }
    (StatusCode::CREATED, Json(info)).into_response()
}

async fn delete_snapshot(
    State(s): State<SharedState>,
    Path(tag): Path<String>,
    Query(q): Query<DeleteSnapshotQuery>,
) -> Response {
    // Sanity-guard the tag before touching disk paths.
    if !is_safe_tag(&tag) {
        return bad_request("tag must be 1-64 chars, ASCII alnum or dash/underscore");
    }
    if q.force && q.cascade {
        return bad_request(
            "force and cascade are mutually exclusive — force orphans children, \
             cascade deletes them; pick one",
        );
    }

    // v0.5 Phase 4: chain-aware safety. Refuse to delete a snapshot
    // that's the recorded parent_tag of one or more children unless
    // the caller opts into either `cascade` (delete the whole subtree)
    // or `force` (orphan the children, leaving them un-restorable).
    let dependents = find_dependents(&s.registry, &s.snapshot_root, &tag);
    if !dependents.is_empty() {
        if q.cascade {
            // Depth-first delete. We recurse via the same handler logic
            // (find_dependents on each child) so a long branch unwinds
            // cleanly. Errors halt the cascade and surface to the caller
            // with the partial state.
            let to_remove = collect_subtree(&s.registry, &s.snapshot_root, &tag);
            for child in to_remove.iter().rev() {
                if child == &tag {
                    continue; // self handled below
                }
                if let Err(msg) = delete_single_snapshot(&s, child) {
                    return server_error(&msg);
                }
            }
        } else if !q.force {
            return conflict(&format!(
                "snapshot `{tag}` is the parent of {n} chained snapshot(s): [{children}]; \
                 rerun with `?cascade=true` to delete the whole subtree, or `?force=true` \
                 to orphan the children (they will fail to restore)",
                n = dependents.len(),
                children = dependents.join(", "),
            ));
        }
        // q.force: fall through, leave dependents alone.
    }

    match delete_single_snapshot(&s, &tag) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => not_found(&format!("snapshot {tag}")),
        Err(msg) => server_error(&msg),
    }
}

/// Inner helper: drop a single snapshot from the registry + filesystem.
/// `Ok(true)` on real delete, `Ok(false)` when nothing was found (the
/// 404 case), `Err(msg)` on a filesystem error worth surfacing as 500.
/// Shared between `delete_snapshot` and the cascade walk so the
/// behavior stays bit-identical.
fn delete_single_snapshot(s: &AppState, tag: &str) -> Result<bool, String> {
    let removed = s
        .registry
        .remove_snapshot(tag)
        .map_err(|e| format!("registry remove: {e}"))?;
    let dir = s.snapshot_root.join(tag);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).map_err(|e| format!("rm {}: {e}", dir.display()))?;
        Ok(true)
    } else if removed.is_some() {
        // Registered but the dir was gone already — treat as a clean
        // delete so the registry no longer points at a phantom path.
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Walk the dependent-tag forest rooted at `tag` in pre-order.
/// Returns tags in [parent, child, grandchild, ...] order so reverse
/// iteration gives leaves-first delete order. Best-effort: corrupt
/// chain entries are skipped silently.
fn collect_subtree(registry: &Registry, snapshot_root: &std::path::Path, tag: &str) -> Vec<String> {
    let mut out = vec![tag.to_string()];
    let mut stack = vec![tag.to_string()];
    let mut seen = std::collections::HashSet::new();
    seen.insert(tag.to_string());
    while let Some(t) = stack.pop() {
        for child in find_dependents(registry, snapshot_root, &t) {
            if seen.insert(child.clone()) {
                out.push(child.clone());
                stack.push(child);
            }
        }
    }
    out
}

/// `GET /v1/snapshots/:tag/info` — chain depth, dependents, sizes.
/// v0.5 Phase 4. Useful to the CLI's `snapshot-info` and to anyone
/// wanting to decide before `rmi`-ing whether they'll orphan children.
async fn snapshot_info(State(s): State<SharedState>, Path(tag): Path<String>) -> Response {
    if !is_safe_tag(&tag) {
        return bad_request("tag must be 1-64 chars, ASCII alnum or dash/underscore");
    }
    let dir = match resolve_snap_dir(&s.registry, &s.snapshot_root, &tag) {
        Ok(d) => d,
        Err(_) => return not_found(&format!("snapshot {tag}")),
    };
    let meta = load_snapshot_with_fallback(&dir);
    let (ancestors, chain_depth) = match compute_ancestors(&s.registry, &s.snapshot_root, &tag) {
        Ok(v) => v,
        Err(e) => return server_error(&format!("compute ancestors for `{tag}`: {e:#}")),
    };
    let dependents = find_dependents(&s.registry, &s.snapshot_root, &tag);
    let memory_logical_bytes = std::fs::metadata(&meta.memory)
        .map(|m| m.len())
        .unwrap_or(0);
    let memory_physical_bytes = physical_bytes(&meta.memory);
    let vmstate_bytes = std::fs::metadata(&meta.vmstate)
        .map(|m| m.len())
        .unwrap_or(0);
    let registry_entry = s.registry.get_snapshot(&tag);
    let created_at_unix = registry_entry.as_ref().map(|r| r.created_at_unix);
    let body = SnapshotInfoDetail {
        tag: tag.clone(),
        dir: dir.display().to_string(),
        created_at_unix,
        memory_logical_bytes,
        memory_physical_bytes,
        vmstate_bytes,
        parent_tag: meta.parent_tag.clone(),
        parent_content_hash: meta.parent_content_hash.clone(),
        chain_depth,
        dependents,
        ancestors,
    };
    Json(body).into_response()
}

/// `POST /v1/snapshots/:tag/compact` — assemble the chain rooted at
/// `tag` into a new flat (parentless) snapshot tagged `req.to`.
/// v0.5 Phase 4. Useful when chain depth has grown enough that the
/// per-link spawn tax (~hash(base size) ms per link on the Phase 5
/// bench) starts to bite.
///
/// The new snapshot's `vmstate` is a copy of the head's vmstate
/// (vmstate is a single-snapshot concept — chains only stack memory).
/// The new `memory.bin` is the chain-assembled image. `parent_tag` is
/// unset on the result, breaking the chain link for future spawn paths.
async fn compact_snapshot(
    State(s): State<SharedState>,
    Path(tag): Path<String>,
    Json(req): Json<CompactSnapshotRequest>,
) -> Response {
    if !is_safe_tag(&tag) {
        return bad_request("source tag must be 1-64 chars, ASCII alnum or dash/underscore");
    }
    if !is_safe_tag(&req.to) {
        return bad_request("to tag must be 1-64 chars, ASCII alnum or dash/underscore");
    }
    if tag == req.to {
        return bad_request("compact target tag must differ from source tag");
    }
    let dest_dir = s.snapshot_root.join(&req.to);
    if dest_dir.exists() {
        return conflict(&format!(
            "compact target `{}` already exists at {}",
            req.to,
            dest_dir.display()
        ));
    }

    // Resolve the source chain end-to-end.
    let registry = s.registry.clone();
    let snapshot_root = s.snapshot_root.clone();
    let head_tag = tag.clone();
    let chain = match forkd_vmm::chain::resolve_chain(&head_tag, move |t| {
        lookup_snapshot_for_chain(&registry, &snapshot_root, t)
    }) {
        Ok(c) => c,
        Err(e) => return server_error(&format!("resolve chain for `{tag}`: {e:#}")),
    };
    // Verify hash integrity before paying for the assemble I/O.
    if let Err(e) = forkd_vmm::chain::verify_parent_hashes(&chain) {
        return server_error(&format!("verify chain for `{tag}`: {e:#}"));
    }
    let head_snapshot = chain.last().map(|(_, s)| s.clone());
    let head_snapshot = match head_snapshot {
        Some(s) => s,
        None => return server_error(&format!("chain for `{tag}` resolved to empty")),
    };

    // Assemble to a temp file, then atomically rename the staging dir
    // into place. This way an interrupted compact never leaves a
    // half-written `req.to` snapshot dir behind.
    let staging = s.snapshot_root.join(format!(".compact-staging-{}", req.to));
    let _ = std::fs::remove_dir_all(&staging);
    if let Err(e) = std::fs::create_dir_all(&staging) {
        return server_error(&format!("create staging {}: {e}", staging.display()));
    }
    let staging_mem = staging.join("memory.bin");
    let assembled_bytes = match forkd_vmm::chain::assemble_chain_memory(&chain, &staging_mem) {
        Ok(b) => b,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&staging);
            return server_error(&format!("assemble chain for `{tag}`: {e:#}"));
        }
    };
    if let Err(e) = std::fs::copy(&head_snapshot.vmstate, staging.join("vmstate")) {
        let _ = std::fs::remove_dir_all(&staging);
        return server_error(&format!(
            "copy vmstate from `{tag}` ({}): {e}",
            head_snapshot.vmstate.display()
        ));
    }
    // Persist the new (flat) snapshot.json so future spawns + chain
    // walks see the compacted result as a base. `parent_tag` is
    // intentionally None — the whole point of compact is to flatten.
    let new_meta = forkd_vmm::Snapshot {
        vmstate: dest_dir.join("vmstate"),
        memory: dest_dir.join("memory.bin"),
        volumes: head_snapshot.volumes.clone(),
        parent_tag: None,
        parent_content_hash: None,
        // Compaction flattens but doesn't change the rootfs — inherit
        // the head's so the compacted base stays hub-portable (#242).
        rootfs: head_snapshot.rootfs.clone(),
        rootfs_read_only: None,
    };
    let staging_meta_path = staging.join("snapshot.json");
    let new_meta_json = match serde_json::to_vec_pretty(&new_meta) {
        Ok(v) => v,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&staging);
            return server_error(&format!("serialize compacted snapshot.json: {e}"));
        }
    };
    if let Err(e) = std::fs::write(&staging_meta_path, new_meta_json) {
        let _ = std::fs::remove_dir_all(&staging);
        return server_error(&format!(
            "write staging snapshot.json {}: {e}",
            staging_meta_path.display()
        ));
    }
    // Atomic-ish promote.
    if let Err(e) = std::fs::rename(&staging, &dest_dir) {
        let _ = std::fs::remove_dir_all(&staging);
        return server_error(&format!(
            "promote staging {} → {}: {e}",
            staging.display(),
            dest_dir.display()
        ));
    }
    // Register the new flat snapshot. `branched_from` carries the
    // source tag so consumers (e.g. `forkd ls --snapshots`) can see
    // the compacted snapshot's lineage without walking parent_tag.
    let snap_info = SnapshotInfo {
        tag: req.to.clone(),
        dir: dest_dir.display().to_string(),
        created_at_unix: unix_now(),
        branched_from: Some(format!("compact:{tag}")),
        pause_ms: None,
        diff_ms: None,
        diff_physical_bytes: None,
        diff_logical_bytes: None,
        warning: None,
        status: crate::api::SnapshotStatus::Ready,
        bootable: true,
    };
    if let Err(e) = s.registry.insert_snapshot(snap_info.clone()) {
        return server_error(&format!("register compacted snapshot: {e}"));
    }
    tracing::info!(
        source_tag = %tag,
        compacted_tag = %req.to,
        chain_depth = chain.len(),
        assembled_bytes,
        "v0.5 chain compacted to flat snapshot"
    );
    Json(snap_info).into_response()
}

fn is_safe_tag(tag: &str) -> bool {
    !tag.is_empty()
        && tag.len() <= 64
        && tag
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Read `<snap_dir>/snapshot.json` into a `forkd_vmm::Snapshot`. If
/// the file is missing or unparseable, construct a base Snapshot
/// pointing at `<snap_dir>/{vmstate,memory.bin}` with empty volumes —
/// the historical pre-snapshot.json fallback. v0.4 and earlier
/// snapshots that lived purely on disk without a metadata file go
/// through this fallback path.
fn load_snapshot_with_fallback(snap_dir: &std::path::Path) -> forkd_vmm::Snapshot {
    std::fs::read(snap_dir.join("snapshot.json"))
        .ok()
        .and_then(|raw| serde_json::from_slice::<forkd_vmm::Snapshot>(&raw).ok())
        .unwrap_or_else(|| forkd_vmm::Snapshot {
            vmstate: snap_dir.join("vmstate"),
            memory: snap_dir.join("memory.bin"),
            volumes: Vec::new(),
            parent_tag: None,
            parent_content_hash: None,
            rootfs: None,
            rootfs_read_only: None,
        })
}

/// Load snapshot metadata while preserving operator-facing parse/read
/// failures. Missing `snapshot.json` is still accepted for older flat
/// snapshots, which are described by the conventional `vmstate` and
/// `memory.bin` filenames.
fn load_snapshot_for_restore(snap_dir: &std::path::Path) -> Result<forkd_vmm::Snapshot, String> {
    let meta_path = snap_dir.join("snapshot.json");
    if meta_path.exists() {
        let raw =
            std::fs::read(&meta_path).map_err(|e| format!("read {}: {e}", meta_path.display()))?;
        serde_json::from_slice::<forkd_vmm::Snapshot>(&raw)
            .map_err(|e| format!("parse {}: {e}", meta_path.display()))
    } else {
        Ok(forkd_vmm::Snapshot {
            vmstate: snap_dir.join("vmstate"),
            memory: snap_dir.join("memory.bin"),
            volumes: Vec::new(),
            parent_tag: None,
            parent_content_hash: None,
            rootfs: None,
            rootfs_read_only: None,
        })
    }
}

fn missing_file_reason(label: &str, path: &std::path::Path) -> Option<String> {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() => None,
        Ok(_) => Some(format!("{label} is not a file: {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Some(format!("missing {label}: {}", path.display()))
        }
        Err(e) => Some(format!("stat {label} {}: {e}", path.display())),
    }
}

fn snapshot_restore_problem(snapshot: &forkd_vmm::Snapshot) -> Option<String> {
    missing_file_reason("vmstate", &snapshot.vmstate)
        .or_else(|| missing_file_reason("memory artifact", &snapshot.memory))
        .or_else(|| {
            snapshot
                .rootfs
                .as_ref()
                .and_then(|rootfs| missing_file_reason("rootfs", rootfs))
        })
}

fn snapshot_unbootable_reason(snap_dir: &std::path::Path) -> Option<String> {
    match load_snapshot_for_restore(snap_dir) {
        Ok(snapshot) => snapshot_restore_problem(&snapshot),
        Err(reason) => Some(reason),
    }
}

/// Opt-in: refuse (409) instead of warning when a snapshot's rootfs is
/// shared writable and a live sandbox already has it open.
///
/// Off by default because refusing rejects `fork -n N` on a writable
/// snapshot, which is the flow `from-image` prints as the next step —
/// making that a hard error is a behaviour change the operator should
/// choose, not one to spring on them. On, it converts the current
/// "one writer at a time" discipline into an enforced invariant instead
/// of scheduling luck.
fn refuse_shared_writable_rootfs() -> bool {
    matches!(
        std::env::var("FORKD_REFUSE_SHARED_RW").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

/// `Some(reason)` when restoring this snapshot would add a second
/// concurrent writer to a rootfs that is not read-only.
///
/// A restored child reopens the rootfs drive at the path AND the
/// read-only flag frozen into the vmstate, and Firecracker offers no way
/// to override either at load time — there is no `drive_overrides` on
/// `/snapshot/load`, and the vmstate is a binary blob rather than JSON.
/// So children of a writable-rootfs snapshot share one ext4: two of them
/// are two guest kernels writing one filesystem with no coordinator,
/// which corrupts package files and directory entries at random and
/// surfaces as a broken build rather than a broken sandbox.
fn shared_writable_rootfs_conflict(
    s: &AppState,
    tag: &str,
    snapshot: &forkd_vmm::Snapshot,
) -> Option<String> {
    // `None` (not recorded, not inferable) cannot be asserted writable,
    // so it does not block. Only a definite writable rootfs does.
    if snapshot.rootfs_is_read_only() != Some(false) {
        return None;
    }
    let live: std::collections::HashSet<String> = s.live_vms.lock().keys().cloned().collect();
    let holders: Vec<String> = s
        .registry
        .list_sandboxes()
        .into_iter()
        .filter(|sb| sb.snapshot_tag == tag && live.contains(&sb.id))
        .map(|sb| sb.id)
        .collect();
    if holders.is_empty() {
        return None;
    }
    Some(format!(
        "snapshot `{tag}` was baked with a WRITABLE rootfs, so every restored child reopens the \
         same ext4 read-write and concurrent children corrupt each other's files. {} live \
         sandbox(es) already hold it ({}). Re-bake the snapshot read-only, or restore one child \
         at a time.",
        holders.len(),
        holders.join(", ")
    ))
}

fn snapshot_info_from_disk_dir(tag: String, dir: PathBuf) -> SnapshotInfo {
    let created_at_unix = dir
        .metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    SnapshotInfo {
        tag,
        dir: dir.display().to_string(),
        created_at_unix,
        branched_from: None,
        pause_ms: None,
        diff_ms: None,
        diff_physical_bytes: None,
        diff_logical_bytes: None,
        warning: None,
        status: crate::api::SnapshotStatus::Ready,
        bootable: true,
    }
}

fn annotate_snapshot_bootability(mut info: SnapshotInfo) -> SnapshotInfo {
    if info.status != crate::api::SnapshotStatus::Ready {
        info.bootable = false;
        return info;
    }
    if let Some(reason) = snapshot_unbootable_reason(std::path::Path::new(&info.dir)) {
        info.status = crate::api::SnapshotStatus::Failed;
        info.bootable = false;
        info.warning = Some(format!("snapshot is not bootable: {reason}"));
    } else {
        info.bootable = true;
    }
    info
}

/// Map a snapshot tag to its on-disk directory using the registry-first,
/// snapshot-root-fallback rule the spawn / branch handlers already
/// share. Returns `Ok(snap_dir)` on success; `Err` when the tag is
/// neither registered nor present in the on-disk XDG location.
///
/// Used by v0.5 chain resolution to look up parent tags by name.
fn resolve_snap_dir(
    registry: &Registry,
    snapshot_root: &std::path::Path,
    tag: &str,
) -> anyhow::Result<PathBuf> {
    if let Some(s) = registry.get_snapshot(tag) {
        return Ok(PathBuf::from(&s.dir));
    }
    let dir = snapshot_root.join(tag);
    if dir.join("vmstate").exists() {
        Ok(dir)
    } else {
        anyhow::bail!(
            "snapshot tag `{tag}` not found in registry or under {}",
            snapshot_root.display()
        )
    }
}

/// Build the `lookup` closure for [`forkd_vmm::chain::resolve_chain`].
/// Given a tag, finds its `snap_dir` and loads the
/// `Snapshot` from `snapshot.json` (or the fallback). v0.4-and-earlier
/// bases without a `snapshot.json` are returned as `parent_tag = None`
/// bases, terminating any chain walk cleanly.
fn lookup_snapshot_for_chain(
    registry: &Registry,
    snapshot_root: &std::path::Path,
    tag: &str,
) -> anyhow::Result<forkd_vmm::Snapshot> {
    let dir = resolve_snap_dir(registry, snapshot_root, tag)?;
    Ok(load_snapshot_with_fallback(&dir))
}

/// Enumerate every snapshot tag the daemon can observe — registered
/// tags from the in-memory registry plus on-disk subdirectories of
/// `snapshot_root`. Used by chain-aware helpers (dependent enumeration,
/// info, compact) so the operations cover snapshots created out-of-band
/// via `forkd snapshot` CLI in addition to daemon-tracked ones.
fn enumerate_all_snapshot_tags(
    registry: &Registry,
    snapshot_root: &std::path::Path,
) -> std::collections::BTreeSet<String> {
    let mut out = std::collections::BTreeSet::new();
    for s in registry.list_snapshots() {
        out.insert(s.tag);
    }
    if let Ok(rd) = std::fs::read_dir(snapshot_root) {
        for entry in rd.flatten() {
            let p = entry.path();
            if !p.join("vmstate").exists() {
                continue;
            }
            if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                if is_safe_tag(name) {
                    out.insert(name.to_string());
                }
            }
        }
    }
    out
}

/// Return every tag whose loaded `snapshot.json` has `parent_tag ==
/// Some(parent)`. Skips tags whose metadata is missing or unparseable
/// (those are v0.4-and-earlier bases and don't carry chain edges).
/// O(N) over all known snapshots — fine at v0.5 scale.
fn find_dependents(
    registry: &Registry,
    snapshot_root: &std::path::Path,
    parent: &str,
) -> Vec<String> {
    let mut out = Vec::new();
    for tag in enumerate_all_snapshot_tags(registry, snapshot_root) {
        if tag == parent {
            continue;
        }
        let Ok(dir) = resolve_snap_dir(registry, snapshot_root, &tag) else {
            continue;
        };
        let meta = load_snapshot_with_fallback(&dir);
        if meta.parent_tag.as_deref() == Some(parent) {
            out.push(tag);
        }
    }
    out
}

/// Walk parent_tag edges from `tag` back to a base snapshot. Returns
/// `Ok((ancestors, depth))` where `ancestors[0]` is the root base and
/// depth is the number of diff links between base and `tag`
/// (== 0 when `tag` is itself a base). Errors only when a parent_tag
/// references a snapshot that doesn't exist (a corrupt chain).
fn compute_ancestors(
    registry: &Registry,
    snapshot_root: &std::path::Path,
    tag: &str,
) -> anyhow::Result<(Vec<String>, usize)> {
    let mut chain_back: Vec<String> = Vec::new();
    let mut current = tag.to_string();
    let mut seen = std::collections::HashSet::new();
    seen.insert(current.clone());
    loop {
        let dir = resolve_snap_dir(registry, snapshot_root, &current)?;
        let meta = load_snapshot_with_fallback(&dir);
        match meta.parent_tag {
            None => break,
            Some(parent) => {
                if !seen.insert(parent.clone()) {
                    anyhow::bail!(
                        "chain cycle detected: snapshot `{tag}` reaches `{parent}` twice"
                    );
                }
                chain_back.push(parent.clone());
                current = parent;
            }
        }
    }
    // chain_back is [parent, grandparent, ..., root]. Reverse for
    // root-first ordering.
    chain_back.reverse();
    let depth = chain_back.len();
    Ok((chain_back, depth))
}

/// Physical on-disk allocation for a regular file, via `st_blocks * 512`
/// (the Linux convention even when the filesystem's block size differs).
/// Returns 0 if stat() fails (e.g. file missing).
fn physical_bytes(path: &std::path::Path) -> u64 {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(path)
            .map(|m| m.blocks() * 512)
            .unwrap_or(0)
    }
    #[cfg(not(target_os = "linux"))]
    {
        std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
    }
}

fn build_snapshot_boot_config(
    kernel: &std::path::Path,
    rootfs: &std::path::Path,
    work_dir: &std::path::Path,
    rw: bool,
    tap: Option<&str>,
) -> forkd_vmm::BootConfig {
    use forkd_vmm::{BootConfig, NetworkConfig};
    // ext4 → writable boot args; otherwise read-only squashfs-style.
    let rootfs_ext4 = rw
        || rootfs
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("ext4"))
            .unwrap_or(false);
    let mut cfg = if rootfs_ext4 {
        BootConfig::ext4_rw(
            kernel.to_path_buf(),
            rootfs.to_path_buf(),
            work_dir.to_path_buf(),
        )
    } else {
        BootConfig::quickstart(
            kernel.to_path_buf(),
            rootfs.to_path_buf(),
            work_dir.to_path_buf(),
        )
    };
    if let Some(t) = tap {
        cfg = cfg.with_network(NetworkConfig::default_tap(t.to_string()));
    }
    cfg
}

async fn create_sandbox(
    State(s): State<SharedState>,
    Json(req): Json<CreateSandboxRequest>,
) -> Response {
    if req.n == 0 {
        return bad_request("n must be ≥ 1");
    }
    if req.n > 1000 {
        return bad_request("n must be ≤ 1000 (sanity cap)");
    }
    // Validate snapshot_tag BEFORE any filesystem op. Without this, a tag
    // like `../../etc` makes `snapshot_root.join(tag)` traverse outside
    // snapshot_root (Rust's Path::join doesn't normalise `..`), and the
    // unvalidated tag would also persist into SandboxInfo.snapshot_tag,
    // later feeding `read_snapshot_volumes` and letting an attacker pick
    // the JSON file the daemon parses for grandchild volume specs.
    if !is_safe_tag(&req.snapshot_tag) {
        return bad_request("snapshot_tag must be 1-64 chars, ASCII alnum or dash/underscore");
    }

    // Snapshot can come either from the daemon's registry (created via
    // future POST /v1/snapshots) or from the on-disk XDG location (created
    // via `forkd snapshot` CLI). Try registry first, then fall back to disk.
    let snap_dir: PathBuf = match s.registry.get_snapshot(&req.snapshot_tag) {
        Some(info) => {
            if info.status != crate::api::SnapshotStatus::Ready {
                let reason = info
                    .warning
                    .as_deref()
                    .unwrap_or("snapshot is not ready for restore");
                return conflict(&format!(
                    "snapshot `{}` is not bootable: status={:?}; {reason}",
                    req.snapshot_tag, info.status
                ));
            }
            PathBuf::from(&info.dir)
        }
        None => {
            let dir = s.snapshot_root.join(&req.snapshot_tag);
            if !dir.exists() {
                return not_found(&format!("snapshot {}", req.snapshot_tag));
            }
            dir
        }
    };
    // Prefer the persisted snapshot.json (carries volumes); fall back
    // to constructing from vmstate + memory.bin for backward compat
    // with snapshots written before the meta file existed.
    let snapshot = match load_snapshot_for_restore(&snap_dir) {
        Ok(snapshot) => snapshot,
        Err(reason) => {
            return conflict(&format!(
                "snapshot `{}` is not bootable: {reason}",
                req.snapshot_tag
            ))
        }
    };
    if let Some(reason) = snapshot_restore_problem(&snapshot) {
        return conflict(&format!(
            "snapshot `{}` is not bootable: {reason}",
            req.snapshot_tag
        ));
    }
    // A writable-rootfs snapshot restored while another child already
    // holds it means two writers on one ext4 — corruption, not a warning
    // (see `shared_writable_rootfs_conflict`). Warn by default; refuse
    // when the operator has opted in.
    if let Some(reason) = shared_writable_rootfs_conflict(&s, &req.snapshot_tag, &snapshot) {
        if refuse_shared_writable_rootfs() {
            return conflict(&reason);
        }
        tracing::warn!(
            snapshot = %req.snapshot_tag,
            "{reason} Set FORKD_REFUSE_SHARED_RW=1 to refuse instead."
        );
    }

    // v0.5: if the loaded snapshot has parent_tag set, this is a
    // chained snapshot. Resolve the chain, verify parent content
    // hashes, and assemble a single memory.bin into a per-spawn
    // scratch path. The assembled file replaces `snapshot.memory`
    // for the rest of this request — FC mmaps it like any other
    // base memory file. The file lives in `work_dir` (computed
    // below) so it inherits the existing work_dir cleanup story.
    //
    // For non-chained snapshots (parent_tag == None) this is a
    // no-op and the historical fast path is preserved bit-for-bit.
    if req.live_fork && snapshot.parent_tag.is_some() {
        // Documented v0.5 carve-out: live_fork on chained sources
        // requires the assembled file to be memfd-populated, plus
        // additional lifetime plumbing (see DESIGN-v0.5 "Live-fork
        // interaction" section). Land as v0.6.
        return bad_request(
            "live_fork=true on a chained snapshot is not yet supported (v0.5 carve-out); \
             spawn with live_fork=false or use a base snapshot",
        );
    }

    let tag = req.snapshot_tag.clone();
    // Reserve the netns offset range INSIDE the spawn_blocking task
    // (review #282 round 3): the old code reserved in the async handler,
    // so if the request future was cancelled while the blocking restore
    // ran, the handler dropped the reservation (making indices available
    // to another request) while the restore task still used them. By
    // owning the reservation in the blocking task, cancellation drops
    // it only after the restore completes or fails.
    let netns_alloc = s.netns_alloc.clone();
    let prewarm_scratch_dir = if req.prewarm {
        Some(s.prewarm_scratch_dir.clone())
    } else {
        None
    };
    let spawn_n = req.n;
    let spawn_per_child_netns = req.per_child_netns;
    let spawn_mem_limit = req.memory_limit_mib;
    let spawn_live_fork = req.live_fork;
    let spawn_hugepages = req.hugepages;
    let tag_for_work_dir = tag.clone();
    // Shared-tap lease (review #281): for per_child_netns=false spawns,
    // claim the shared host tap INSIDE spawn_blocking so handler
    // cancellation cannot release it mid-restore. The claim is returned
    // UNCOMMITTED and committed AFTER live_vms.insert() so a cancelled
    // handler between restore success and registration does not wedge
    // the tap permanently. The token is the first child's sandbox id;
    // release_shared_tap_if_owner checks against it on teardown.
    let use_shared_tap = !req.per_child_netns;
    let tap_owner_cell = s.shared_tap_owner.clone();
    let tap_token = if use_shared_tap {
        Some(new_sandbox_id())
    } else {
        None
    };
    // Reject shared-TAP batches above n=1 (review #281 round 3): when
    // per_child_netns=false, all children share a single host tap fd
    // (forkd-tap0). The tap lease is owned by the first child's sandbox
    // id; deleting that child releases the lease while sibling VMs
    // remain live on the same tap fd, causing EBUSY on the next
    // shared-tap spawn. The common case is n=1; n>1 shared-TAP spawns
    // are rejected with 503 until per-child tap ownership is modeled.
    if use_shared_tap && req.n > 1 {
        return service_unavailable(
            "shared-TAP spawns with n>1 are not supported; use per_child_netns=true for multi-child spawns",
        );
    }

    // v0.5 chain resolution. Resolve the chain BEFORE spawn_blocking
    // because it only reads snapshot.json files (cheap). The expensive
    // step (hash verify + reflink + apply_diff) runs inside the
    // spawn_blocking below so it doesn't starve the async runtime.
    // For non-chained snapshots, `chain` stays None and the historical
    // fast path is preserved bit-for-bit.
    let chain = if snapshot.parent_tag.is_some() {
        let registry = s.registry.clone();
        let snapshot_root = s.snapshot_root.clone();
        let head_tag = req.snapshot_tag.clone();
        match forkd_vmm::chain::resolve_chain(&head_tag, move |t| {
            lookup_snapshot_for_chain(&registry, &snapshot_root, t)
        }) {
            Ok(c) => Some(c),
            Err(e) => {
                return server_error(&format!("resolve chain for `{}`: {e:#}", req.snapshot_tag))
            }
        }
    } else {
        None
    };

    // restore_many_with is sync + blocking (spawns N firecracker procs,
    // waits on their unix sockets, fires N parallel restore PUTs). Run it
    // off the async runtime so we don't starve other requests.
    //
    // Retry-on-busy: when a sandbox is killed and another spawn fires
    // immediately, the kernel's tap-device / cgroup teardown can race
    // with the new firecracker process trying to claim them, producing:
    //   - "Open tap device failed: Resource busy (os error 16)" — fc
    //     can't open forkd-tap0 because the previous owner's fd hasn't
    //     been released
    //   - "Device or resource busy" on cgroup leaf creation
    // The kernel usually clears state within tens of milliseconds. Retry
    // up to 3 times with 50/200/800 ms backoff. ForkOpts and Snapshot are
    // Clone so we can hand a fresh copy to each attempt.
    // Boot identity for sandbox registration (review #299): read it
    // BEFORE any VM work so an environment where boot_id is unreadable
    // fails the spawn loudly here instead of recording a silently-
    // unverifiable row that would abort every future controller start.
    let reg_boot_id = match registration_boot_id() {
        Ok(b) => b,
        Err(e) => return server_error(&format!("{e:#}")),
    };
    let prewarm_requested = req.prewarm;
    let mut snapshot = snapshot;
    let head_tag_for_log = req.snapshot_tag.clone();
    // #301: surface orphaned firecracker processes before the restore.
    // The helper scopes the live_vms lock to just PID collection and
    // runs the /proc scan + logging after the lock is released.
    //
    // Limitation: this scan is point-in-time and does NOT guarantee that
    // no orphan appears between the scan and the restore. It is a
    // diagnostic aid, not a gate. Issue #301 remains open for the
    // lifecycle-level orphan reap/gate work.
    warn_orphan_firecrackers(&s, &head_tag_for_log);
    let tap_token_for_closure = tap_token.clone();
    let (fork_result, mut netns_reservation, mut tap_claim) = match tokio::task::spawn_blocking(move || -> anyhow::Result<(forkd_vmm::ForkResult, Option<crate::netns::NetnsReservation>, Option<SharedTapClaim>)> {
        // Reserve the netns offset range INSIDE the blocking task so
        // the reservation survives async-handler cancellation. If
        // the handler is dropped while this task runs, the task still
        // holds the reservation; when the task completes, its return
        // value is dropped, which drops the uncommitted reservation and
        // releases the reserved indices back to the pool. A concurrent
        // spawn can never grab the same range because the indices stay
        // in the reserved set until the task finishes (review #282 r3).
        let netns_reservation = if spawn_per_child_netns {
            match netns_alloc.reserve(spawn_n) {
                Some(r) => Some(r),
                None => return Err(anyhow::Error::new(NetnsExhausted)),
            }
        } else {
            None
        };
        // Claim the shared tap (if this spawn uses it) before restore.
        // A second live shared-tap VM is rejected with SharedTapBusy ->
        // 503; the claim is returned UNCOMMITTED and committed by the
        // async handler after live_vms.insert() so handler cancellation
        // between restore success and registration does not wedge the
        // tap permanently (review #281).
        let tap_claim = if use_shared_tap {
            match try_claim_shared_tap(
                &tap_owner_cell,
                tap_token_for_closure.expect("use_shared_tap => token"),
            ) {
                Some(c) => Some(c),
                None => return Err(anyhow::Error::new(SharedTapBusy)),
            }
        } else {
            None
        };
        let netns_offset = netns_reservation.as_ref().map(|r| r.offset()).unwrap_or(0);
        let opts = forkd_vmm::ForkOpts {
            n: spawn_n,
            per_child_netns: spawn_per_child_netns,
            memory_limit_mib: spawn_mem_limit,
            netns_offset,
            prewarm_scratch_dir,
            // Phase 6 unstable: live_fork=true opts the sandbox into
            // memfd-backed RAM so the Phase 6 mode=live BRANCH path can
            // arm UFFD_WP on it. Default stays File for backward compat.
            memory_backend: if spawn_live_fork {
                forkd_vmm::MemoryBackend::MemfdShared {
                    use_hugepages: spawn_hugepages,
                }
            } else {
                forkd_vmm::MemoryBackend::File
            },
            // Daemon-spawned sources are the targets of BRANCH; enabling
            // dirty-page tracking lets later BRANCHes opt into Diff
            // snapshots (see docs/design/diff-snapshots.md). The cost is
            // ~1 bit per page; negligible.
            enable_diff_snapshots: true,
            sync_guest_clocks: true,
            // Under spawn bursts (warm-pool refill) the default 10s socket
            // wait can be too tight when prior-VM tap/netns teardown races
            // with new firecracker spawns. Give the daemon path a more
            // generous budget so a single slow child doesn't fail the batch.
            socket_wait_timeout_secs: daemon_socket_wait_timeout_secs(),

        };
        // Per-snapshot-tag work_dir would clash if two batches of the same tag
        // ran concurrently (e.g. two branches of the same source). Mix the
        // netns offset in so concurrent batches get distinct work_dirs.
        let work_dir =
            std::env::temp_dir().join(format!("forkd-daemon-{tag_for_work_dir}-o{netns_offset}"));
        // v0.5 chain assembly. If `chain` is set, hash-verify each
        // parent against the recorded parent_content_hash (this is
        // the foot-gun guard committed to in the design doc), then
        // assemble `cp(base) + apply_diff(each)` into a per-spawn
        // file under work_dir. Replace `snapshot.memory` with the
        // assembled path so the rest of the flow is unchanged.
        if let Some(chain) = chain {
            // v0.5 chain assembly must NOT live as a flat file inside
            // `work_dir`: `restore_many_with` sweeps every non-dir entry
            // in `work_dir` on entry (to clear stale FC sockets / console
            // files between spawns). A flat `memory-assembled.bin` would
            // get unlinked between assemble and FC's /snapshot/load,
            // yielding "No such file" (os error 2) from Firecracker.
            //
            // Fix: stage the assembled file under a `chainstage/`
            // SUBDIRECTORY of work_dir. The sweep loop has an explicit
            // `if p.is_dir() { continue; }` so subdirectories are
            // preserved. Keeping it under work_dir means standard work_dir
            // cleanup (on sandbox delete) drops the assembled file with it.
            let chainstage_dir = work_dir.join("chainstage");
            std::fs::create_dir_all(&chainstage_dir).with_context(|| {
                format!(
                    "create chainstage dir {} for chain assembly",
                    chainstage_dir.display()
                )
            })?;
            forkd_vmm::chain::verify_parent_hashes(&chain)
                .with_context(|| format!("verify chain for `{head_tag_for_log}`"))?;
            let assembled = chainstage_dir.join("memory-assembled.bin");
            // If a previous spawn for the same tag/offset left an
            // assembled file behind (e.g. crash mid-spawn), remove
            // it so create_new in assemble_chain_memory succeeds.
            let _ = std::fs::remove_file(&assembled);
            let bytes = forkd_vmm::chain::assemble_chain_memory(&chain, &assembled)
                .with_context(|| format!("assemble chain for `{head_tag_for_log}`"))?;
            tracing::info!(
                tag = %head_tag_for_log,
                chain_depth = chain.len(),
                assembled_bytes = bytes,
                assembled_path = %assembled.display(),
                "v0.5 chain assembled for spawn"
            );
            snapshot.memory = assembled;
        }

        let backoffs_ms = [50u64, 200, 800];
        // Tracks the most recent busy failure across retries. On the final
        // attempt the loop falls through (rather than returning early) so
        // the exhausted-retry error carries retry-exhaustion context into
        // the API response/log — operators can then distinguish a transient
        // contention storm from a permanent failure.
        let mut last_err: Option<anyhow::Error> = None;
        for attempt in 0..=backoffs_ms.len() {
            if attempt > 0 {
                std::thread::sleep(std::time::Duration::from_millis(
                    backoffs_ms[attempt - 1],
                ));
            }
            let final_attempt = attempt == backoffs_ms.len();
            match snapshot.restore_many_with(opts.clone(), &work_dir) {
                Ok(r) if r.failures.is_empty() => return Ok((r, netns_reservation, tap_claim)),
                Ok(r) => {
                    // Partial success: some children restored, some failed.
                    // A sandbox needs all `n` children, so drop the partial
                    // set (kills their firecracker processes) and retry the
                    // whole batch when the failure is retryable (busy);
                    // otherwise surface the structured per-child failures.
                    //
                    // Controller-level all-or-nothing: a sandbox requires
                    // all N children, so partial success is NOT exposed to
                    // the API caller. The per-child failure reporting from
                    // restore_many_with is used here for diagnostics and
                    // retry classification only. See issue #301 for the
                    // broader partial-success-through-controller work.
                    let forkd_vmm::ForkResult {
                        children,
                        failures,
                        ..
                    } = r;
                    let is_busy = failures.iter().any(|f| {
                        f.error.contains("Resource busy")
                            || f.error.contains("Device or resource busy")
                            || f.error.contains("os error 16")
                    });
                    let err = anyhow::Error::new(forkd_vmm::RestoreError { failures });
                    drop(children);
                    if !is_busy {
                        return Err(err);
                    }
                    if final_attempt {
                        last_err =
                            Some(err.context("restore_many: exhausted all busy-retries"));
                    } else {
                        tracing::warn!(
                            attempt = attempt + 1,
                            next_backoff_ms = backoffs_ms[attempt],
                            error = %err,
                            "restore_many: partial restore (tap/cgroup busy), retrying"
                        );
                        last_err = Some(err);
                    }
                }
                Err(e) => {
                    let msg = format!("{e:#}");
                    let is_busy = msg.contains("Resource busy")
                        || msg.contains("Device or resource busy")
                        || msg.contains("os error 16");
                    if !is_busy {
                        return Err(e);
                    }
                    if final_attempt {
                        last_err =
                            Some(e.context("restore_many: exhausted all busy-retries"));
                    } else {
                        last_err = Some(e);
                    }
                    tracing::warn!(
                        attempt = attempt + 1,
                        next_backoff_ms = backoffs_ms[attempt],
                        error = %msg,
                        "restore_many: tap/cgroup busy, retrying"
                    );
                }
            }
        }
        // All busy-retries exhausted; surface the final error (carrying
        // retry-exhaustion context) for the API response/log.
        Err(last_err.expect("busy-retry loop exhausted without capturing an error"))
    })
    .await
    {
        Ok(Ok((fr, nr, tc))) => (fr, nr, tc),
        Ok(Err(e)) if e.downcast_ref::<NetnsExhausted>().is_some() => {
            return service_unavailable(&format!("{e:#}"));
        }
        Ok(Err(e)) if e.downcast_ref::<SharedTapBusy>().is_some() => {
            return service_unavailable(&format!("{e:#}"));
        }
        Ok(Err(e)) => return server_error(&format!("restore_many: {e:#}")),
        Err(e) => return server_error(&format!("blocking task panicked: {e}")),
    };
    if prewarm_requested {
        let clock_failures: Vec<String> = fork_result
            .clock_sync_outcomes
            .iter()
            .filter_map(|o| match o {
                ClockSyncOutcome::Failed { child_index, error } => {
                    Some(format!("child {child_index}: {error}"))
                }
                _ => None,
            })
            .collect();
        tracing::info!(
            tag = %tag,
            n = fork_result.children.len(),
            spawn_ms = fork_result.spawn_ms as u64,
            restore_ms = fork_result.restore_ms as u64,
            prewarm_ms = fork_result.prewarm_ms as u64,
            clock_sync_ms = fork_result.clock_sync_ms as u64,
            clock_sync_failures =? clock_failures,
            "sandbox created (prewarmed)"
        );
    }

    let now = unix_now();
    let mut infos = Vec::with_capacity(fork_result.children.len());
    let mut first_id_override = tap_token.clone();
    {
        let mut live = s.live_vms.lock();
        for fc in fork_result.children {
            let vm = fc.vm;
            // Shared-tap ownership (review #281): the first child's
            // sandbox id is the tap-lease token; release_shared_tap_if_owner
            // checks against it on teardown. For non-shared-tap spawns
            // first_id_override is None so every child gets a fresh id.
            let id = first_id_override.take().unwrap_or_else(new_sandbox_id);
            let info = SandboxInfo {
                id: id.clone(),
                snapshot_tag: tag.clone(),
                netns: vm.netns.clone(),
                // Currently all children share the parent snapshot's MAC/IP.
                // Per-child netns isolates them on the wire; same address
                // is fine because each netns is its own broadcast domain.
                guest_addr: "10.42.0.2:8888".to_string(),
                created_at_unix: now,
                pid: Some(vm.pid()),
                proc_starttime: read_proc_starttime(vm.pid()),
                boot_id: reg_boot_id.clone(),
                memory_limit_mib: req.memory_limit_mib,
                has_branched: false,
                last_branch_memory_path: None,
                branch_count: 0,
            };
            if let Err(e) = s.registry.insert_sandbox(info.clone()) {
                tracing::error!(error=%e, "persist sandbox failed");
            }
            live.insert(id, vm);
            infos.push(info);
        }
        // Registration complete: the netns reservation transfers to
        // live_vms (the VM entries now own the indices). If the loop
        // above is never reached (spawn failure), the reservation drops
        // and releases automatically.
        if let Some(res) = netns_reservation.as_mut() {
            res.commit();
        }
        // Commit the shared-tap lease AFTER live_vms.insert() (review #281):
        // committing inside spawn_blocking would wedge the tap permanently
        // if the handler is cancelled between restore success and
        // registration.
        if let Some(claim) = tap_claim.as_mut() {
            claim.commit();
        }
    }

    (StatusCode::CREATED, Json(infos)).into_response()
}

async fn delete_sandbox(State(s): State<SharedState>, Path(id): Path<String>) -> Response {
    // Remove the VM from live_vms, kill firecracker (Vm::drop), then
    // release the netns index and shared-tap lease. Killing before
    // releasing closes the window where a new spawn could enter
    // forkd-child-N or grab forkd-tap0 while the previous owner is
    // still dying (review #281 & #282).
    let vm = s.live_vms.lock().remove(&id);
    let netns = vm.as_ref().and_then(|v| v.netns.clone());
    let is_shared_tap = vm.as_ref().map(|v| v.netns.is_none()).unwrap_or(false);
    drop(vm); // kills firecracker + cleans cgroup
    release_netns_index(&s, netns.as_deref());
    release_shared_tap_if_owner(&s, &id, is_shared_tap);
    let registered = match s.registry.remove_sandbox(&id) {
        Ok(v) => v,
        Err(e) => return server_error(&format!("registry remove: {e}")),
    };
    if registered.is_none() {
        return not_found(&format!("sandbox {id}"));
    }
    StatusCode::NO_CONTENT.into_response()
}

/// Release a netns index from a `forkd-child-N` string. Called after
/// the firecracker process has been killed (Vm::drop) so the index only
/// becomes reusable once the namespace is vacated (review #282).
fn release_netns_index(s: &SharedState, netns: Option<&str>) {
    if let Some(ns) = netns {
        if let Some(idx) = ns.strip_prefix("forkd-child-") {
            if let Ok(n) = idx.parse::<usize>() {
                s.netns_alloc.release_index(n);
            } else {
                tracing::warn!(
                    netns = ns,
                    "failed to parse netns index; index leaks from active set"
                );
            }
        }
    }
}

/// Guard that kills the VM and releases its netns index on drop.
/// Ensures netns cleanup survives async-handler cancellation: when the
/// handler future is dropped while a spawn_blocking task is running, the
/// task's return value (which contains this guard) is dropped, triggering
/// cleanup — kill firecracker, then release the netns index (review #282 r3).
struct VmNetnsGuard {
    vm: Option<forkd_vmm::Vm>,
    netns_index: Option<usize>,
    alloc: std::sync::Arc<crate::netns::NetnsAllocator>,
    /// Shared-tap cleanup: set when the VM uses the shared host tap
    /// (netns is None). On Drop, clears the owner if it matches
    /// `tap_owner_id` (review #281 r3 + #282 r3).
    shared_tap_owner: Option<std::sync::Arc<parking_lot::Mutex<Option<String>>>>,
    tap_owner_id: Option<String>,
}

impl VmNetnsGuard {
    fn new(
        vm: forkd_vmm::Vm,
        alloc: std::sync::Arc<crate::netns::NetnsAllocator>,
        shared_tap_owner: Option<std::sync::Arc<parking_lot::Mutex<Option<String>>>>,
        tap_owner_id: Option<String>,
    ) -> Self {
        let netns_index = vm.netns.as_ref().and_then(|ns| {
            ns.strip_prefix("forkd-child-")
                .and_then(|idx| idx.parse().ok())
        });
        // Only carry shared-tap cleanup info if the VM actually uses
        // the shared tap (netns is None) and both the owner cell and
        // owner id were provided.
        let uses_shared_tap = vm.netns.is_none();
        let (shared_tap_owner, tap_owner_id) = if uses_shared_tap {
            (shared_tap_owner, tap_owner_id)
        } else {
            (None, None)
        };
        Self {
            vm: Some(vm),
            netns_index,
            alloc,
            shared_tap_owner,
            tap_owner_id,
        }
    }

    /// Take the VM out of the guard without releasing the netns index
    /// or shared-tap lease. Call this when transferring ownership to
    /// live_vms (both stay owned until the VM is later deleted/suspended).
    ///
    /// The guard then drops normally — its remaining fields (the
    /// `Arc<NetnsAllocator>`, the shared-tap owner Arc, and bookkeeping
    /// strings) are released without leaking. `Drop` checks
    /// `self.vm.is_some()` so the netns index and shared-tap lease are
    /// NOT released when the VM was already taken out here.
    fn into_vm(mut self) -> forkd_vmm::Vm {
        self.vm.take().expect("guard already consumed")
        // self drops here; Drop sees vm == None and skips kill+release.
    }
}

impl std::ops::Deref for VmNetnsGuard {
    type Target = forkd_vmm::Vm;
    fn deref(&self) -> &Self::Target {
        self.vm.as_ref().expect("guard already consumed")
    }
}

impl std::ops::DerefMut for VmNetnsGuard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.vm.as_mut().expect("guard already consumed")
    }
}

#[cfg(test)]
impl VmNetnsGuard {
    /// Construct a guard in the post-`into_vm()` state: the VM has
    /// already been taken out, but the bookkeeping fields (the
    /// `Arc<NetnsAllocator>` and netns index) remain. This lets tests
    /// verify that dropping such a guard releases the Arc reference
    /// without leaking — the regression for the `mem::forget` bug.
    fn test_consumed(alloc: std::sync::Arc<crate::netns::NetnsAllocator>) -> Self {
        Self {
            vm: None,
            netns_index: Some(1),
            alloc,
            shared_tap_owner: None,
            tap_owner_id: None,
        }
    }
}

impl Drop for VmNetnsGuard {
    fn drop(&mut self) {
        // Only kill the VM and release resources when the guard
        // still owns the VM. If `into_vm()` was called, `self.vm` is
        // `None` — the VM was transferred to `live_vms` and the index
        // and shared-tap lease must stay owned until that VM is later
        // deleted/suspended.
        if self.vm.is_some() {
            // Vm::drop kills firecracker + cleans cgroup
            self.vm.take();
            // Release the netns index after the VM is killed
            if let Some(idx) = self.netns_index {
                self.alloc.release_index(idx);
            }
            // Release the shared-tap lease after the VM is killed
            if let (Some(cell), Some(id)) = (&self.shared_tap_owner, &self.tap_owner_id) {
                let mut owner = cell.lock();
                if owner.as_deref() == Some(id) {
                    *owner = None;
                }
            }
        }
        // The Arc<NetnsAllocator> (and other bookkeeping fields) drop
        // normally, decrementing refcounts. No leak.
    }
}

/// `POST /v1/sandboxes/:id/branch` — pause a running sandbox, snapshot its
/// memory + vmstate to a new tag, resume it. The resulting snapshot is
/// independent of the source sandbox's lifecycle: it can be forked from
/// or deleted later regardless of whether the source is still alive.
///
/// While the snapshot is being written the source sandbox is paused at
/// the vCPU level (kernel state and TCP sockets remain; application-level
/// keepalives may time out). Typical pause window: 0.5–8 s depending on
/// the memory image size.
///
/// Implementation note: we take the `Vm` out of `live_vms` for the duration
/// of the blocking pause/snapshot/resume sequence, then put it back. This
/// avoids holding the mutex during the slow operation, at the cost of a
/// short window where the sandbox is invisible to `list_sandboxes` /
/// `delete_sandbox`.
async fn branch_sandbox(
    State(s): State<SharedState>,
    Path(id): Path<String>,
    Json(req): Json<BranchSandboxRequest>,
) -> Response {
    let tag = req
        .tag
        .clone()
        .unwrap_or_else(|| format!("branch-{}-{}-{:04x}", id, unix_now(), branch_suffix()));
    if !is_safe_tag(&tag) {
        return bad_request("tag must be 1-64 chars, ASCII alnum or dash/underscore");
    }

    // Acquire concurrency slot before any disk check. The slot covers both
    // (a) per-tag exclusion (two BRANCHes on the same tag would otherwise
    // race past the vmstate-exists check and clobber memory.bin) and
    // (b) global cap (each BRANCH may write multiple GiB; uncapped concurrency
    // is a disk-fill DoS). Held via RAII so every early-return below releases
    // it for free.
    let _slot = match s.try_acquire_branch_slot(&tag) {
        Ok(slot) => slot,
        Err(BranchSlotError::AlreadyInFlight) => {
            return conflict(&format!(
                "branch for tag '{tag}' is already in progress; retry once it completes"
            ));
        }
        Err(BranchSlotError::CapacityExceeded) => {
            return service_unavailable(&format!(
                "daemon is at its branch concurrency cap ({}); retry shortly",
                DEFAULT_BRANCH_CONCURRENCY
            ));
        }
    };

    // Phase 7: canonical `mode` takes precedence over the legacy
    // `diff` / `live` bools. Allowing both opens an ambiguity
    // window — reject up front.
    if req.mode.is_some() && (req.diff || req.live) {
        return bad_request(
            "set either `mode` (canonical) OR the legacy `diff` / `live` booleans, not both",
        );
    }
    let (effective_diff, effective_live) = match req.mode {
        Some(crate::api::BranchMode::Full) => (false, false),
        Some(crate::api::BranchMode::Diff) => (true, false),
        Some(crate::api::BranchMode::Live) => (false, true),
        None => (req.diff, req.live),
    };
    // Phase 6.3: mode selection — legacy bools still mutually
    // exclusive with `measure_diff`. (When `mode` is set, the legacy
    // bools are forced to `false` above, so the check still applies
    // via `effective_*`.)
    let mode_count = req.measure_diff as u8 + effective_diff as u8 + effective_live as u8;
    if mode_count > 1 {
        return bad_request(
            "set at most one of `measure_diff` / `diff` / `live` (or use `mode`): \
             measure_diff is the pure measurement hook (Full path + Diff sidecar timing); \
             diff is the real diff-based BRANCH path; \
             live is the v0.4 UFFD_WP-based path",
        );
    }
    // Phase 6.4: wait=false is the async live path; meaningless for
    // Full/Diff (they're already synchronous by construction).
    if !req.wait && !effective_live {
        return bad_request("`wait: false` requires `live: true` (or `mode: \"live\"`)");
    }

    let snap_dir = s.snapshot_root.join(&tag);
    if snap_dir.join("vmstate").exists() {
        return conflict(&format!("snapshot {} already exists; DELETE first", tag));
    }

    // Look up the source sandbox's snapshot_tag so we can inherit its volumes
    // into the branch. Branches without inherited volumes wouldn't be able to
    // re-attach the parent's persistent disks on restore.
    let (source_snapshot_tag, source_last_branch_memory) = match s.registry.get_sandbox(&id) {
        Some(info) => (info.snapshot_tag, info.last_branch_memory_path),
        None => return not_found(&format!("sandbox {id}")),
    };

    // v0.5 chain edge — when `parent_tag` is set in the request, the
    // resulting snapshot.json records itself as a diff-snapshot chain
    // link (`parent_tag` + `parent_content_hash`). The chain resolver
    // (`forkd_vmm::chain::resolve_chain`) walks this at restore time.
    //
    // Defensive validation: `parent_tag` must equal the source's
    // `snapshot_tag` AND the BRANCH must be in diff mode. Other shapes
    // would record a chain edge that doesn't match the actual memory
    // derivation, breaking restores. The CLI verb `forkd snapshot diff`
    // is the canonical caller; raw REST callers get the same guard.
    if let Some(ref pt) = req.parent_tag {
        if pt != &source_snapshot_tag {
            return bad_request(&format!(
                "parent_tag `{pt}` must equal the source sandbox's snapshot_tag \
                 `{source_snapshot_tag}` — v0.5 chain edges only record the immediate \
                 parent the source was spawned from"
            ));
        }
        if !effective_diff {
            return bad_request(
                "parent_tag requires diff mode (set `mode: \"diff\"` or legacy `diff: true`) — \
                 Full BRANCH writes the whole memory so chaining has no semantic meaning; \
                 Live BRANCH on chained sources is carved out to v0.6",
            );
        }
    }
    // Phase 1d: multi-BRANCH diff is supported. For diff: true requests,
    // we pick the cp source as follows:
    //   - If the sandbox's last_branch_memory_path is set AND the file
    //     still exists, use it (the previous BRANCH's output is, by
    //     construction, source's state at that BRANCH's pause time —
    //     exactly the base the next diff needs).
    //   - Otherwise (first BRANCH, or user deleted the previous snapshot),
    //     fall back to the source tag's memory.bin (source's boot state).
    //     The fallback is semantically lossy when the chain was broken
    //     by deletion, but it's the only sensible behavior — we log a
    //     warning so operators can see when this happens.
    let source_volumes = match read_snapshot_volumes(&s.snapshot_root, &source_snapshot_tag) {
        Ok(v) => v,
        Err(e) => {
            return server_error(&format!(
                "read source snapshot volumes from tag '{source_snapshot_tag}': {e:#}"
            ));
        }
    };

    // Take the VM out of live_vms briefly; we'll put it back unconditionally
    // (even on failure) unless a concurrent DELETE on the same id happened.
    // Wrap in VmNetnsGuard so the netns index is released even if the
    // handler is cancelled or a concurrent DELETE removes the registry
    // entry during the BRANCH take-out window (review #282 r3).
    let vm = {
        let mut g = s.live_vms.lock();
        g.remove(&id)
    };
    let vm = match vm {
        Some(v) => v,
        None => return not_found(&format!("sandbox {id}")),
    };
    let vm = VmNetnsGuard::new(
        vm,
        s.netns_alloc.clone(),
        Some(s.shared_tap_owner.clone()),
        Some(id.clone()),
    );

    let snap_dir_for_task = snap_dir.clone();
    let id_for_log = id.clone();
    let measure_diff = req.measure_diff;
    let diff_mode = effective_diff;
    let live_mode = effective_live;
    let source_tag_memory_path = s
        .snapshot_root
        .join(&source_snapshot_tag)
        .join("memory.bin");
    // Phase 1d: pick the cp source for diff mode. Prefer the previous
    // BRANCH output (chain), fall back to source tag (first BRANCH OR
    // chain broken by user-side deletion).
    let (source_memory_path, chain_broken) = match source_last_branch_memory {
        Some(p) if p.exists() => (p, false),
        Some(p) => {
            tracing::warn!(
                sandbox = %id,
                stale_path = %p.display(),
                "diff BRANCH: last_branch_memory_path missing on disk, falling back to source tag (chain broken — output may miss pages dirtied before deletion)"
            );
            (source_tag_memory_path.clone(), true)
        }
        None => (source_tag_memory_path.clone(), false),
    };
    let _ = chain_broken; // reserved for future telemetry; intentionally unused today
    type DiffMetrics = Option<(u64, u64, u64)>; // (ms, physical_bytes, logical_bytes)
    let req_wait = req.wait;
    // Box the LiveBranchWorker on non-Linux so the tuple type stays sized
    // even though the worker can never be Some there.
    #[cfg(target_os = "linux")]
    type LiveWorkerSlot = Option<LiveBranchWorker>;
    #[cfg(not(target_os = "linux"))]
    type LiveWorkerSlot = Option<()>;
    let task_result = tokio::task::spawn_blocking(
        move || -> (
            VmNetnsGuard,
            anyhow::Result<forkd_vmm::Snapshot>,
            Option<u64>,
            DiffMetrics,
            LiveWorkerSlot,
        ) {
            let mut pause_ms: Option<u64> = None;
            let mut diff_metrics: DiffMetrics = None;
            let mut live_worker_out: LiveWorkerSlot = None;
            let snap_result = (|| -> anyhow::Result<forkd_vmm::Snapshot> {
                std::fs::create_dir_all(&snap_dir_for_task)?;

                // Issue #146 fix: pre-allocate the destination
                // memory.bin to the source's full size. ext4's delayed
                // allocator otherwise runs mballoc + block-bitmap CRC
                // on every write range, compounding per BRANCH and
                // causing the ~5× pause_ms jump on BRANCH 3+. Best-
                // effort: on tmpfs / unsupported FS the syscall returns
                // ENOSYS, we log and continue.
                let dst_mem = snap_dir_for_task.join("memory.bin");
                if let Ok(meta) = std::fs::metadata(&source_memory_path) {
                    let src_size = meta.len();
                    if src_size > 0 {
                        if let Err(e) = preallocate_memory_file(&dst_mem, src_size) {
                            tracing::warn!(
                                sandbox = %id_for_log,
                                size = src_size,
                                error = %e,
                                "preallocate memory.bin failed (continuing without it)"
                            );
                        }
                    }
                }

                // Phase 6.3 live-fork path: WP-arm the source's memfd
                // VMA (via FC), pause briefly to dump vmstate, resume,
                // then stream memory.bin from the controller's mmap of
                // the same memfd while the source keeps running. No
                // background source-copy here — bulk_copy_clean does
                // the equivalent work directly from the memfd through
                // our mmap.
                #[cfg(target_os = "linux")]
                if live_mode {
                    let (live_pause_ms, worker) = run_live_branch_setup(
                        &vm,
                        &snap_dir_for_task,
                        &dst_mem,
                        source_volumes.clone(),
                        &id_for_log,
                    )?;
                    pause_ms = Some(live_pause_ms);
                    if req_wait {
                        let stats = worker.drive_bulk_copy()?;
                        tracing::info!(
                            sandbox = %id_for_log,
                            pause_ms = live_pause_ms,
                            wp_arm_us = stats.arm_duration.as_micros() as u64,
                            captured_by_fault = stats.pages_captured_by_fault,
                            captured_by_bulk = stats.pages_captured_by_bulk,
                            total_pages = stats.total_pages,
                            "branch: live-mode (sync) pause/copy/finalize complete"
                        );
                    } else {
                        // wait=false: hand the worker off so the outer
                        // post-task code can spawn it on a background
                        // thread and stash the JoinHandle in
                        // AppState::live_in_flight.
                        live_worker_out = Some(worker);
                    }
                    return Ok(forkd_vmm::Snapshot {
                        vmstate: snap_dir_for_task.join("vmstate"),
                        memory: dst_mem.clone(),
                        volumes: source_volumes,
                        parent_tag: None,
                        parent_content_hash: None,
                        rootfs: None,
                        rootfs_read_only: None,
                    });
                }
                #[cfg(not(target_os = "linux"))]
                if live_mode {
                    anyhow::bail!(
                        "live BRANCH (Phase 6.3) is Linux-only — userfaultfd is a Linux syscall"
                    );
                }

                // Phase 1b: if `diff` mode, kick off a background copy of
                // the source tag's memory.bin → snap_dir/memory.bin BEFORE
                // we pause. The source runs concurrently. After the diff
                // snapshot finishes (fast) and we resume, we join the
                // copy and apply the diff onto its output. Source's pause
                // window collapses to just the diff_ms.
                //
                // The cp falls into the file we just pre-allocated, so
                // the copy doesn't trigger mballoc.
                let copy_handle: Option<std::thread::JoinHandle<std::io::Result<u64>>> =
                    if diff_mode {
                        let src = source_memory_path.clone();
                        let dst = dst_mem.clone();
                        Some(std::thread::spawn(move || std::fs::copy(&src, &dst)))
                    } else {
                        None
                    };

                let pause_start = std::time::Instant::now();
                vm.pause()?;

                // Phase 1a measurement hook: take a Diff snapshot first
                // (captures pages dirtied since restore; clears the dirty
                // bitmap). Discarded after metrics. The subsequent Full
                // snapshot still writes every page, so the post-resume
                // snapshot state is unchanged.
                if measure_diff {
                    let diff_dir = std::env::temp_dir()
                        .join(format!("forkd-diff-measure-{}", std::process::id()));
                    std::fs::create_dir_all(&diff_dir)
                        .context("create diff measurement scratch dir")?;
                    let diff_vmstate = diff_dir.join("diff-vmstate");
                    let diff_mem = diff_dir.join("diff-memory.bin");
                    let diff_start = std::time::Instant::now();
                    let diff_snap = vm
                        .snapshot_diff_to(
                            diff_vmstate.clone(),
                            diff_mem.clone(),
                            Vec::new(),
                        )
                        .context("diff snapshot")?;
                    let diff_ms = diff_start.elapsed().as_millis() as u64;
                    diff_metrics = Some((
                        diff_ms,
                        diff_snap.physical_size_bytes,
                        diff_snap.logical_size_bytes,
                    ));
                    // Discard the diff files — they were measurement-only.
                    let _ = std::fs::remove_file(&diff_vmstate);
                    let _ = std::fs::remove_file(&diff_mem);
                    let _ = std::fs::remove_dir(&diff_dir);
                }

                let snap = if diff_mode {
                    // Diff path: take a Diff snapshot into a temp file,
                    // resume the source, then merge the diff onto the
                    // pre-copied snap_dir/memory.bin.
                    let diff_path = std::env::temp_dir().join(format!(
                        "forkd-branch-diff-{}-{}.bin",
                        std::process::id(),
                        unix_now()
                    ));
                    let diff_start = std::time::Instant::now();
                    let diff_snap = vm
                        .snapshot_diff_to(
                            snap_dir_for_task.join("vmstate"),
                            diff_path.clone(),
                            source_volumes.clone(),
                        )
                        .context("diff snapshot (diff mode)")?;
                    let diff_ms = diff_start.elapsed().as_millis() as u64;
                    diff_metrics = Some((
                        diff_ms,
                        diff_snap.physical_size_bytes,
                        diff_snap.logical_size_bytes,
                    ));
                    let resume_result = vm.resume();
                    pause_ms = Some(pause_start.elapsed().as_millis() as u64);

                    // Wait for the background memory.bin copy to finish.
                    let copy_bytes = copy_handle
                        .expect("copy_handle set in diff_mode")
                        .join()
                        .map_err(|e| anyhow::anyhow!("copy thread panicked: {:?}", e))?
                        .context("copy source memory.bin to snap_dir")?;
                    tracing::debug!(
                        sandbox = %id_for_log,
                        copy_bytes,
                        diff_physical_bytes = diff_snap.physical_size_bytes,
                        "diff-branch: source memory copy done"
                    );

                    // Apply the diff onto the snap_dir/memory.bin in place.
                    let merged_bytes =
                        forkd_vmm::apply_diff(&diff_path, &snap_dir_for_task.join("memory.bin"))
                            .context("apply_diff onto snap_dir memory")?;
                    let _ = std::fs::remove_file(&diff_path);
                    tracing::info!(
                        sandbox = %id_for_log,
                        pause_ms = pause_ms.unwrap_or(0),
                        diff_ms,
                        diff_physical_bytes = diff_snap.physical_size_bytes,
                        merged_bytes,
                        "branch: diff-mode pause/resume + merge complete"
                    );
                    if let Err(e) = resume_result {
                        tracing::warn!(
                            sandbox = %id_for_log,
                            error = %e,
                            "branch: source failed to resume after diff snapshot; snapshot file is intact"
                        );
                    }
                    // Return a normal Snapshot pointing at the merged
                    // memory.bin so the downstream Registry/serialization
                    // path is unchanged.
                    forkd_vmm::Snapshot {
                        vmstate: diff_snap.vmstate,
                        memory: snap_dir_for_task.join("memory.bin"),
                        volumes: diff_snap.volumes,
                        parent_tag: None,
                        parent_content_hash: None,
                        rootfs: None,
                        rootfs_read_only: None,
                    }
                } else {
                    let snap = vm.snapshot_to(
                        snap_dir_for_task.join("vmstate"),
                        snap_dir_for_task.join("memory.bin"),
                        // Inherit volumes from the source snapshot so grandchildren
                        // re-attach the same persistent disks the source had.
                        source_volumes,
                    )?;
                    // resume() may fail after a successful snapshot. The snapshot file
                    // is intact and usable; the source sandbox is in an unknown state
                    // (most likely still paused). We log and continue rather than
                    // returning Err, because the user's primary expectation (a valid
                    // new snapshot) has been met.
                    let resume_result = vm.resume();
                    pause_ms = Some(pause_start.elapsed().as_millis() as u64);
                    if let Err(e) = resume_result {
                        tracing::warn!(
                            sandbox = %id_for_log,
                            pause_ms = pause_ms.unwrap_or(0),
                            error = %e,
                            "branch: source sandbox failed to resume after snapshot; snapshot file is intact"
                        );
                    } else if let Some((dms, dphys, dlog)) = diff_metrics {
                        tracing::info!(
                            sandbox = %id_for_log,
                            pause_ms = pause_ms.unwrap_or(0),
                            diff_ms = dms,
                            diff_physical_bytes = dphys,
                            diff_logical_bytes = dlog,
                            "branch: source paused/resumed cleanly (with diff measurement)"
                        );
                    } else {
                        tracing::info!(
                            sandbox = %id_for_log,
                            pause_ms = pause_ms.unwrap_or(0),
                            "branch: source paused/resumed cleanly"
                        );
                    }
                    snap
                };
                Ok(snap)
            })();
            (vm, snap_result, pause_ms, diff_metrics, live_worker_out)
        },
    )
    .await;

    let (vm_guard, snap_or_err, pause_ms, diff_metrics, live_worker) = match task_result {
        Ok(t) => t,
        Err(e) => {
            // Blocking task panicked; the guard's Drop already ran during
            // the panic unwind, killing firecracker and releasing the netns
            // index. No further cleanup needed.
            return server_error(&format!("blocking task panicked: {e}"));
        }
    };

    // Re-insert the source sandbox into live_vms. If a DELETE happened during
    // the branching window, the entry is gone from the registry; dropping the
    // guard kills firecracker and releases the netns index (review #282 r3).
    let mut new_branch_count: Option<u32> = None;
    if s.registry.get_sandbox(&id).is_some() {
        s.live_vms.lock().insert(id.clone(), vm_guard.into_vm());
        // Phase 1d: record this BRANCH's memory.bin as the chain head
        // for the next diff BRANCH. Both Full and Diff modes clear the
        // dirty bitmap, so EITHER mode's output is the correct base for
        // the next diff regardless of which mode produced it.
        let new_chain_head = snap_dir.join("memory.bin");
        match s.registry.mark_branched(&id, new_chain_head) {
            Ok(count) => new_branch_count = count,
            Err(e) => {
                tracing::warn!(sandbox = %id, error = %e, "failed to persist last_branch_memory_path");
            }
        }
    } else {
        // DELETE happened during BRANCH: guard's Drop kills firecracker
        // and releases the netns index (review #282 r3).
        drop(vm_guard);
    }

    let mut snap = match snap_or_err {
        Ok(s) => s,
        Err(e) => {
            // Best-effort cleanup of partial files.
            let _ = std::fs::remove_dir_all(&snap_dir);
            return server_error(&format!("branch: {e:#}"));
        }
    };

    // v0.5 chain edge — record parent_tag + sha256 of the parent's
    // current memory file in the new snapshot.json. Hashing happens
    // here (not inside the spawn_blocking) because we already paid
    // for the source restore + diff write; one more file hash is a
    // small marginal cost and keeps the chain-edge logic localized
    // to the BRANCH handler instead of threading through Vm.
    if let Some(parent_tag) = req.parent_tag.clone() {
        // The parent's memory file is whatever its snapshot.json
        // points at — `memory.bin` for a base, `diff.bin` for an
        // already-chained parent. Both work; the content-hash pins
        // whichever bytes the chain actually references.
        let parent_snap_dir = s.snapshot_root.join(&parent_tag);
        let parent_snap = load_snapshot_with_fallback(&parent_snap_dir);
        let parent_hash = match forkd_vmm::chain::sha256_file(&parent_snap.memory) {
            Ok(h) => h,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&snap_dir);
                return server_error(&format!(
                    "hash parent `{parent_tag}` memory at {}: {e:#}",
                    parent_snap.memory.display()
                ));
            }
        };
        snap.parent_tag = Some(parent_tag);
        snap.parent_content_hash = Some(parent_hash);
    }

    // Persist snapshot.json (matches the create_snapshot path).
    let meta = match serde_json::to_vec_pretty(&snap) {
        Ok(v) => v,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&snap_dir);
            return server_error(&format!("serialize snapshot.json: {e}"));
        }
    };
    if let Err(e) = std::fs::write(snap_dir.join("snapshot.json"), &meta) {
        let _ = std::fs::remove_dir_all(&snap_dir);
        return server_error(&format!("write snapshot.json: {e}"));
    }

    let (diff_ms, diff_physical_bytes, diff_logical_bytes) = match diff_metrics {
        Some((ms, phys, log)) => (Some(ms), Some(phys), Some(log)),
        None => (None, None, None),
    };
    // No warning emitted post-v0.3.4: the multi-BRANCH pause anomaly
    // that originally motivated this surface (#146) was fixed by the
    // posix_fallocate path in this same handler. `branch_count` stays
    // in SandboxInfo as a diagnostic counter; `warning` stays in the
    // SnapshotInfo schema (with skip_serializing_if = None) so future
    // BRANCH-specific advisories can populate it without an API break.
    let _ = new_branch_count;
    let warning: Option<String> = None;

    // Phase 6.4: wait=false live BRANCH. The blocking task returned a
    // LiveBranchWorker instead of running bulk_copy_clean + finalize
    // inline. Spawn that work on a std::thread now, register the join
    // handle in AppState::live_in_flight so list_snapshots can reap +
    // promote on subsequent calls, and respond 202 Accepted with the
    // Writing snapshot record. The synchronous Ready path below stays
    // for everything else.
    #[cfg(target_os = "linux")]
    if let Some(worker) = live_worker {
        let inflight_info = SnapshotInfo {
            tag: tag.clone(),
            dir: snap_dir.display().to_string(),
            created_at_unix: unix_now(),
            branched_from: Some(id.clone()),
            pause_ms,
            diff_ms,
            diff_physical_bytes,
            diff_logical_bytes,
            warning: warning.clone(),
            status: crate::api::SnapshotStatus::Writing,
            bootable: false,
        };
        let tag_for_log = tag.clone();
        let join = std::thread::spawn(move || {
            let r = worker.drive_bulk_copy();
            if let Err(ref e) = r {
                tracing::warn!(
                    tag = %tag_for_log,
                    error = %e,
                    "live BRANCH background bulk-copy failed; snapshot will be marked Failed on next reap",
                );
            }
            r
        });
        s.live_in_flight.lock().insert(
            tag.clone(),
            LiveBranchHandle {
                info: inflight_info.clone(),
                join,
            },
        );
        return (StatusCode::ACCEPTED, Json(inflight_info)).into_response();
    }
    // Non-Linux: live_worker can never be Some (its slot type is
    // Option<()> there and the cfg gate above ensures we never construct
    // a real worker). Discard it to silence unused-variable warnings.
    #[cfg(not(target_os = "linux"))]
    let _ = live_worker;

    let info = SnapshotInfo {
        tag: tag.clone(),
        dir: snap_dir.display().to_string(),
        created_at_unix: unix_now(),
        branched_from: Some(id.clone()),
        pause_ms,
        diff_ms,
        diff_physical_bytes,
        diff_logical_bytes,
        warning,
        // Sync path (Full / Diff / live with wait:true) — snapshot is
        // already complete on disk.
        status: crate::api::SnapshotStatus::Ready,
        bootable: true,
    };
    if let Err(e) = s.registry.insert_snapshot(info.clone()) {
        return server_error(&format!("persist snapshot: {e:#}"));
    }
    (StatusCode::CREATED, Json(info)).into_response()
}

/// Read the volumes list from a tagged snapshot's `snapshot.json` on disk.
/// Returns an empty Vec if `snapshot.json` is missing (some legacy snapshots
/// don't have it) — that matches the pre-volumes behaviour.
fn read_snapshot_volumes(
    snapshot_root: &std::path::Path,
    tag: &str,
) -> anyhow::Result<Vec<forkd_vmm::VolumeSpec>> {
    // Defense in depth: every caller is expected to have validated `tag` via
    // `is_safe_tag` before persisting it, but if a future caller forgets, or
    // a registry row gets reconstructed from an older state.json written
    // before tag validation existed, refuse to dereference the join.
    if !is_safe_tag(tag) {
        anyhow::bail!("refusing to read snapshot with unsafe tag (defense in depth)");
    }
    let path = snapshot_root.join(tag).join("snapshot.json");
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = std::fs::read_to_string(&path)?;
    let snap: forkd_vmm::Snapshot = serde_json::from_str(&raw)?;
    Ok(snap.volumes)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// #301: per-child firecracker socket-wait timeout for daemon-spawned
/// sandboxes, in seconds. Under warm-pool refill bursts the historical
/// 10s budget can be too tight when prior-VM tap/netns teardown races
/// with new firecracker spawns. Operators can override via
/// `FORKD_SOCKET_WAIT_TIMEOUT_SECS`; the default (30) gives a single
/// slow child more room before the batch fails.
fn daemon_socket_wait_timeout_secs() -> u64 {
    std::env::var("FORKD_SOCKET_WAIT_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&v| v > 0)
        .unwrap_or(30)
}

fn branch_suffix() -> u16 {
    use std::sync::atomic::{AtomicU16, Ordering};
    static SEQ: AtomicU16 = AtomicU16::new(0);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

/// Short, URL-safe sandbox id. Not crypto-random; the daemon-only loopback
/// surface doesn't need unguessable ids. Keep the counter suffix fixed-width
/// so logs and dashboard parsers don't see the id shape drift over uptime.
/// Switch to ULID if we ever expose the API beyond localhost.
fn new_sandbox_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let ts = unix_now();
    format!("sb-{ts:x}-{n:016x}")
}

async fn exec_sandbox(
    State(s): State<SharedState>,
    Path(id): Path<String>,
    Json(req): Json<ExecRequest>,
) -> Response {
    let (addr, netns) = match s.registry.get_sandbox(&id) {
        Some(info) => (info.guest_addr, info.netns),
        None => return not_found(&format!("sandbox {id}")),
    };
    if req.args.is_empty() {
        return bad_request("args must contain at least one element");
    }
    let timeout = std::time::Duration::from_secs(req.timeout_secs);
    let args = req.args;
    let result = tokio::task::spawn_blocking(move || match netns {
        Some(ns) => forkd_vmm::exec_in_netns(&ns, addr, args, timeout),
        None => forkd_vmm::exec_at(&addr, args, timeout),
    })
    .await;
    match result {
        Ok(Ok(r)) => Json(ExecResponse {
            stdout: r.stdout,
            stderr: r.stderr,
            exit_code: r.exit_code.into(),
        })
        .into_response(),
        Ok(Err(e)) => server_error(&format!("exec: {e:#}")),
        Err(e) => server_error(&format!("blocking task panicked: {e}")),
    }
}

async fn eval_sandbox(
    State(s): State<SharedState>,
    Path(id): Path<String>,
    Json(req): Json<EvalRequest>,
) -> Response {
    let (addr, netns) = match s.registry.get_sandbox(&id) {
        Some(info) => (info.guest_addr, info.netns),
        None => return not_found(&format!("sandbox {id}")),
    };
    let code = req.code;
    let result = tokio::task::spawn_blocking(move || match netns {
        Some(ns) => forkd_vmm::eval_in_netns(&ns, addr, code),
        None => forkd_vmm::eval_at(&addr, code),
    })
    .await;
    match result {
        Ok(Ok(v)) => {
            let exit_code = v.get("exit_code").and_then(|x| x.as_i64()).unwrap_or(0);
            let result_field = v.get("result").and_then(|x| x.as_str()).map(String::from);
            let error_field = v.get("error").and_then(|x| x.as_str()).map(String::from);
            Json(EvalResponse {
                result: result_field,
                error: error_field,
                exit_code,
            })
            .into_response()
        }
        Ok(Err(e)) => server_error(&format!("eval: {e:#}")),
        Err(e) => server_error(&format!("blocking task panicked: {e}")),
    }
}

async fn ping_sandbox(State(s): State<SharedState>, Path(id): Path<String>) -> Response {
    let (addr, netns) = match s.registry.get_sandbox(&id) {
        Some(info) => (info.guest_addr, info.netns),
        None => return not_found(&format!("sandbox {id}")),
    };
    let result = tokio::task::spawn_blocking(move || match netns {
        Some(ns) => forkd_vmm::ping_in_netns(&ns, addr),
        None => forkd_vmm::ping_at(&addr),
    })
    .await;
    match result {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => server_error(&format!("ping: {e:#}")),
        Err(e) => server_error(&format!("blocking task panicked: {e}")),
    }
}

fn not_found(what: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(ErrorBody {
            error: format!("{what} not found"),
        }),
    )
        .into_response()
}

/// Pre-allocate a file to `size` bytes using `posix_fallocate(3)`.
///
/// Background (issue #146): when BRANCH writes a fresh memory.bin
/// of 512 MiB+ to ext4, the filesystem's delayed allocation +
/// multi-block allocator + block-bitmap CRC + writeback throttle
/// (`wbt_wait`) compound per BRANCH. After ~3 BRANCHes on the same
/// source, pause_ms jumps 5× (280 → 1400 ms).
///
/// `posix_fallocate` reserves the extents up-front. ext4 marks the
/// space allocated immediately; subsequent writes from FC don't run
/// `ext4_mb_new_blocks` and don't update the block bitmap. Confirmed
/// via the tmpfs control in PROBE-multi-branch-anomaly.md (round 5):
/// pause stays flat at ~150 ms when ext4 isn't the bottleneck.
///
/// Best-effort: failure (e.g. on tmpfs which doesn't support
/// fallocate, or on a filesystem without the syscall) is logged at
/// WARN and the BRANCH continues — semantically a no-op.
#[cfg(unix)]
/// Phase 6.4 worker: owns the controller-side mmap of the memfd plus
/// the WpBranch the fault handler is running on. Sent to a background
/// thread for the `wait: false` live BRANCH path so the HTTP response
/// can return as soon as the source has resumed.
#[cfg(target_os = "linux")]
pub struct LiveBranchWorker {
    // Only used via its Drop impl (the WpBranch's bulk_copy_clean
    // reads through this mmap by raw address). The dead_code lint
    // doesn't notice that.
    #[allow(dead_code)]
    mmap_guard: MmapGuard,
    wp_branch: forkd_uffd::wp_snapshot::WpBranch,
}

/// SAFETY: `mmap_guard.ptr` is owned by this struct (no aliasing). `WpBranch`
/// itself owns the rest of the shared state (uffd `OwnedFd`, fault handler
/// `JoinHandle`, `Arc<SharedState>` over `Mutex<File>` + atomics) — all of
/// which are individually `Send`. The raw pointer in `MmapGuard` is only
/// dereferenced from inside the worker's `drive_bulk_copy()` while the worker
/// is in a single thread at a time; the WpBranch handler thread reads via
/// the same mmap address but does so concurrently with the bulk copier
/// regardless of which thread owns the worker, so transferring ownership
/// doesn't change the synchronization story.
#[cfg(target_os = "linux")]
unsafe impl Send for LiveBranchWorker {}

#[cfg(target_os = "linux")]
impl LiveBranchWorker {
    /// Drive `bulk_copy_clean` then `finalize`. Consumes the worker;
    /// the mmap is `munmap`'d when the guard drops at the end.
    pub fn drive_bulk_copy(self) -> anyhow::Result<forkd_uffd::wp_snapshot::WpBranchStats> {
        // SAFETY: the mmap is alive (held by `self.mmap_guard`) for the
        // duration of this method, including across `bulk_copy_clean`.
        let _copied = unsafe { self.wp_branch.bulk_copy_clean() }
            .context("bulk-copy clean pages out of memfd into snap memory.bin")?;
        self.wp_branch
            .finalize()
            .context("finalize WP branch (stop handler thread)")
        // mmap_guard drops here, releasing the controller-side mapping.
    }
}

/// RAII for a `mmap`/`munmap` pair so any error path in the live
/// BRANCH setup correctly releases the controller-side mapping.
#[cfg(target_os = "linux")]
pub struct MmapGuard {
    ptr: *mut libc::c_void,
    size: usize,
}

#[cfg(target_os = "linux")]
impl Drop for MmapGuard {
    fn drop(&mut self) {
        if !self.ptr.is_null() && self.size > 0 {
            // SAFETY: `ptr`/`size` came from a successful `mmap` in
            // `run_live_branch_setup`.
            unsafe { libc::munmap(self.ptr, self.size) };
        }
    }
}

/// Phase 6.3/6.4 live-fork path setup: arm UFFD_WP on the source's
/// guest memory via the vendored FC's `/uffd/wp` endpoint, take a
/// vmstate-only snapshot inside a tight pause window, return the
/// pause duration along with a [`LiveBranchWorker`] that owns the
/// post-pause bulk copy work. The caller drives the worker either
/// synchronously (Phase 6.3 `wait: true`) or on a background thread
/// (Phase 6.4 `wait: false`).
///
/// Preconditions:
///   - `vm` was spawned with `MemoryBackend::MemfdShared` (Phase 5b).
///     If `vm.memfd_handle()` is None, the sandbox is file-backed
///     and `UFFDIO_REGISTER (WP)` will be refused by the kernel.
///   - The vendored FC binary supports `PUT /uffd/wp` (Phase 6.1.5,
///     commit `7d80afade` on `forkd-v0.4-mem-backend-shared-v1.12`).
///
/// Errors are returned without resuming the VM only in the
/// `PauseGuard` window — the guard's Drop catches the rest.
#[cfg(target_os = "linux")]
fn run_live_branch_setup(
    vm: &forkd_vmm::Vm,
    snap_dir: &std::path::Path,
    dst_mem: &std::path::Path,
    source_volumes: Vec<forkd_vmm::VolumeSpec>,
    id_for_log: &str,
) -> anyhow::Result<(u64, LiveBranchWorker)> {
    use std::os::fd::AsRawFd;

    let memfd = vm.memfd_handle().ok_or_else(|| {
        anyhow::anyhow!(
            "live BRANCH requires a memfd-backed sandbox \
             (MemoryBackend::MemfdShared, Phase 5b); this one is file-backed"
        )
    })?;
    let region_size: usize = memfd.size_bytes().try_into().with_context(|| {
        format!(
            "memfd region size {} doesn't fit in usize",
            memfd.size_bytes()
        )
    })?;
    if region_size == 0 {
        anyhow::bail!("live BRANCH: memfd region is empty");
    }
    // Own a separate File handle to the same memfd so the controller
    // can both mmap it AND pass it into WpBranch as a keepalive.
    let memfd_for_mmap = memfd
        .try_clone()
        .context("dup memfd for controller-side mmap")?;
    let memfd_for_wp = memfd
        .try_clone()
        .context("dup memfd for WpBranch keepalive")?;

    // mmap the memfd in this process. MAP_SHARED so guest writes
    // visible to FC are also visible here — that's the whole point.
    // SAFETY: memfd_for_mmap is an open File; mmap with PROT_READ |
    // PROT_WRITE and MAP_SHARED is the standard pattern for a
    // controller-side view of a shared memfd. region_size is non-zero.
    let region_ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            region_size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            memfd_for_mmap.as_raw_fd(),
            0,
        )
    };
    if region_ptr == libc::MAP_FAILED {
        anyhow::bail!(
            "mmap controller-side memfd ({} bytes): {}",
            region_size,
            std::io::Error::last_os_error()
        );
    }
    let mmap_guard = MmapGuard {
        ptr: region_ptr,
        size: region_size,
    };

    // Ask FC for a WP-uffd. FC will UFFDIO_REGISTER inside its own
    // process, then SCM_RIGHTS the fd back to us.
    let wp_sock = snap_dir.join(".wp.sock");
    let handshake = vm
        .request_wp_uffd(&wp_sock)
        .context("PUT /uffd/wp + receive uffd via SCM_RIGHTS")?;
    if handshake.regions.is_empty() {
        anyhow::bail!("FC returned 0 regions in the WP handshake");
    }

    // Spin up WpBranch around the externally-registered uffd. This
    // arms UFFDIO_WRITEPROTECT (sub-millisecond) and starts the fault
    // handler thread.
    // SAFETY: region_ptr/region_size point at a valid mmap of
    // memfd_for_wp in this process and survive until _mmap_guard +
    // wp_branch drop. The uffd was registered against FC's mmap of
    // the same memfd inode, so events from KVM guest writes fire
    // here.
    let wp_branch = unsafe {
        // FC's address (where UFFDIO_REGISTER applied) for UFFD ops;
        // controller's mmap pointer (`region_ptr`) for read access.
        // The handshake's first region covers the whole guest memory
        // on the x86_64 / mem ≤ 3 GiB layout we ship today.
        let fc_region_addr = handshake.regions[0].base_host_virt_addr as usize;
        forkd_uffd::wp_snapshot::WpBranch::begin_with_external_uffd(
            handshake.uffd,
            memfd_for_wp.into(),
            fc_region_addr,
            region_ptr,
            region_size,
            dst_mem,
        )?
    };

    // Tight critical section: pause -> snapshot_vmstate_only ->
    // resume. PauseGuard's Drop resumes on early return.
    let pause_start = std::time::Instant::now();
    let pause_guard = vm.pause_guard()?;
    vm.snapshot_vmstate_only(
        snap_dir.join("vmstate"),
        dst_mem.to_path_buf(),
        source_volumes,
    )
    .context("vmstate-only snapshot during live BRANCH")?;
    pause_guard.resume().context("resume after vmstate dump")?;
    let pause_ms = pause_start.elapsed().as_millis() as u64;

    tracing::debug!(
        sandbox = %id_for_log,
        pause_ms,
        "branch: live-mode setup done, vmstate written; handing bulk-copy to worker",
    );

    Ok((
        pause_ms,
        LiveBranchWorker {
            mmap_guard,
            wp_branch,
        },
    ))
}

fn preallocate_memory_file(path: &std::path::Path, size: u64) -> anyhow::Result<()> {
    use std::os::fd::AsRawFd;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .with_context(|| format!("open for fallocate: {}", path.display()))?;
    // posix_fallocate(fd, offset, len) — len must fit in off_t (i64).
    let len: libc::off_t = size
        .try_into()
        .map_err(|_| anyhow::anyhow!("memory.bin size {size} doesn't fit in off_t"))?;
    let rc = unsafe { libc::posix_fallocate(file.as_raw_fd(), 0, len) };
    if rc != 0 {
        // posix_fallocate returns the errno directly (not via errno),
        // but ENOSYS / EOPNOTSUPP are non-fatal — tmpfs and some other
        // filesystems just don't support it.
        let err = std::io::Error::from_raw_os_error(rc);
        anyhow::bail!("posix_fallocate({size}): {err}");
    }
    Ok(())
}

fn bad_request(msg: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(ErrorBody {
            error: msg.to_string(),
        }),
    )
        .into_response()
}

fn conflict(msg: &str) -> Response {
    (
        StatusCode::CONFLICT,
        Json(ErrorBody {
            error: msg.to_string(),
        }),
    )
        .into_response()
}

fn server_error(msg: &str) -> Response {
    tracing::error!("internal error: {msg}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorBody {
            error: msg.to_string(),
        }),
    )
        .into_response()
}

fn service_unavailable(msg: &str) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(ErrorBody {
            error: msg.to_string(),
        }),
    )
        .into_response()
}

// -----------------------------------------------------------------------
// Stateful workspaces (#116)
// -----------------------------------------------------------------------

/// Marker error for netns pool exhaustion in the workspace spawn path.
/// Callers downcast this to return 503 (service_unavailable) instead of
/// a generic 500, mirroring the sandbox path's early-return (review
/// #282 round 2).
#[derive(Debug)]
struct NetnsExhausted;

impl std::fmt::Display for NetnsExhausted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "netns pool exhausted: every provisioned forkd-child-N namespace is in use \
             (run scripts/netns-setup.sh N with a larger N and restart)",
        )
    }
}

impl std::error::Error for NetnsExhausted {}

/// Marker error for "the shared host tap is already owned by a live VM".
/// Mapped to 503 by callers (vs. generic 500 for other spawn failures).
#[derive(Debug)]
struct SharedTapBusy;

impl std::fmt::Display for SharedTapBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "shared host tap forkd-tap0 is in use by another live sandbox; \
             delete it first or use per_child_netns=true for parallel sandboxes"
        )
    }
}

impl std::error::Error for SharedTapBusy {}

/// RAII claim on the single shared host tap (`forkd-tap0`).
///
/// Held from inside the `spawn_blocking` restore task until the VM is
/// registered in `live_vms` (commit) or the spawn fails (drop). Only
/// used for `per_child_netns=false` spawns; per-netns spawns each have
/// their own tap and never claim.
///
/// Holds an `Arc` to the owner cell so the claim can cross the
/// `spawn_blocking` boundary and be committed from the async handler
/// after `live_vms.insert()` — while the blocking restore is still
/// running, the claim lives inside the blocking task, so cancelling the
/// async handler cannot release it mid-restore. The commit happens
/// AFTER registration (review #281: commit-before-registration could
/// wedge the tap permanently if the handler is cancelled between
/// restore success and `live_vms.insert`).
pub struct SharedTapClaim {
    owner: std::sync::Arc<parking_lot::Mutex<Option<String>>>,
    token: String,
    committed: bool,
}

impl SharedTapClaim {
    fn commit(&mut self) {
        self.committed = true;
    }
}

impl Drop for SharedTapClaim {
    fn drop(&mut self) {
        if !self.committed {
            let mut owner = self.owner.lock();
            if owner.as_deref() == Some(self.token.as_str()) {
                *owner = None;
            }
        }
    }
}

/// Try to claim the shared host tap. Returns an owner-leased guard, or
/// `None` when another shared-tap VM is already live (the caller maps
/// that to a 503). Called from inside `spawn_blocking`.
fn try_claim_shared_tap(
    owner_cell: &std::sync::Arc<parking_lot::Mutex<Option<String>>>,
    token: impl Into<String>,
) -> Option<SharedTapClaim> {
    let token = token.into();
    let mut owner = owner_cell.lock();
    if owner.is_some() {
        return None;
    }
    *owner = Some(token.clone());
    Some(SharedTapClaim {
        owner: std::sync::Arc::clone(owner_cell),
        token,
        committed: false,
    })
}

/// Release the shared tap lease when a VM leaves `live_vms`. Only
/// shared-tap VMs (`netns == None`) hold the lease. Checks that the
/// departing VM's sandbox id matches the stored owner token before
/// clearing (defense-in-depth: the single-owner invariant should
/// guarantee this, but the check prevents accidental release if a
/// future code path inserts a netns=None VM without claiming the lease).
fn release_shared_tap_if_owner(s: &SharedState, sandbox_id: &str, is_shared_tap: bool) {
    if is_shared_tap {
        let mut owner = s.shared_tap_owner.lock();
        if owner.as_deref() == Some(sandbox_id) {
            *owner = None;
        }
    }
}

/// Spawn one sandbox from a snapshot tag and return the resulting
/// `forkd_vmm::Vm` + the daemon-side metadata, without inserting into
/// the live_vms / Registry. Workspace endpoints insert into those
/// themselves after wrapping the Vm in a WorkspaceInfo. Kept small
/// because the workspace path doesn't need per_child_netns or
/// memory_limit auto-negotiation today.
fn spawn_one_for_workspace(
    s: &SharedState,
    snapshot_tag: &str,
    per_child_netns: bool,
    memory_limit_mib: Option<u64>,
) -> anyhow::Result<(
    forkd_vmm::Vm,
    SandboxInfo,
    Option<crate::netns::NetnsReservation>,
    Option<SharedTapClaim>,
)> {
    // Boot identity for registration (review #299): fail the spawn
    // loudly if boot_id is unreadable rather than recording a row that
    // can never pass startup identity verification.
    let reg_boot_id = registration_boot_id()?;
    let snap_dir: PathBuf = match s.registry.get_snapshot(snapshot_tag) {
        Some(s) => PathBuf::from(&s.dir),
        None => s.snapshot_root.join(snapshot_tag),
    };
    if !snap_dir.join("vmstate").exists() {
        anyhow::bail!("snapshot {snapshot_tag} not found");
    }
    let snapshot = match std::fs::read(snap_dir.join("snapshot.json"))
        .ok()
        .and_then(|raw| serde_json::from_slice::<forkd_vmm::Snapshot>(&raw).ok())
    {
        Some(s) => s,
        None => forkd_vmm::Snapshot {
            vmstate: snap_dir.join("vmstate"),
            memory: snap_dir.join("memory.bin"),
            volumes: Vec::new(),
            parent_tag: None,
            parent_content_hash: None,
            rootfs: None,
            rootfs_read_only: None,
        },
    };
    let netns_reservation = if per_child_netns {
        match s.netns_alloc.reserve(1) {
            Some(r) => Some(r),
            None => {
                return Err(anyhow::Error::new(NetnsExhausted));
            }
        }
    } else {
        None
    };
    let netns_offset = netns_reservation.as_ref().map(|r| r.offset()).unwrap_or(0);
    // Generate the sandbox id early so it can serve as the shared-tap
    // owner token (release_shared_tap_if_owner checks against this id).
    let sandbox_id = new_sandbox_id();
    // Shared-tap lease (review #281): claim INSIDE this function (which
    // runs in spawn_blocking) so handler cancellation cannot release it
    // mid-restore. The claim is returned uncommitted; the caller commits
    // it AFTER live_vms.insert() so a cancelled handler between restore
    // success and registration does not wedge the tap permanently.
    let tap_claim = if !per_child_netns {
        match try_claim_shared_tap(&s.shared_tap_owner, &sandbox_id) {
            Some(c) => Some(c),
            None => return Err(anyhow::Error::new(SharedTapBusy)),
        }
    } else {
        None
    };
    let opts = forkd_vmm::ForkOpts {
        n: 1,
        per_child_netns,
        memory_limit_mib,
        netns_offset,
        prewarm_scratch_dir: None,
        memory_backend: forkd_vmm::MemoryBackend::File,
        enable_diff_snapshots: true,
        sync_guest_clocks: true,
        socket_wait_timeout_secs: daemon_socket_wait_timeout_secs(),
    };
    let work_dir =
        std::env::temp_dir().join(format!("forkd-workspace-{snapshot_tag}-o{netns_offset}"));
    // #301: surface orphaned firecracker processes before the restore.
    warn_orphan_firecrackers(s, snapshot_tag);
    let mut fork_result = snapshot.restore_many_with(opts, &work_dir)?;
    let vm = fork_result.children.pop().map(|fc| fc.vm).ok_or_else(|| {
        if let Some(first) = fork_result.failures.first() {
            anyhow::anyhow!(
                "restore_many child {} failed ({:?}): {}",
                first.child_index,
                first.phase,
                first.error
            )
        } else {
            anyhow::anyhow!("restore_many returned no children")
        }
    })?;

    let info = SandboxInfo {
        id: sandbox_id,
        snapshot_tag: snapshot_tag.to_string(),
        netns: vm.netns.clone(),
        guest_addr: "10.42.0.2:8888".to_string(),
        created_at_unix: unix_now(),
        pid: Some(vm.pid()),
        proc_starttime: read_proc_starttime(vm.pid()),
        boot_id: reg_boot_id,
        memory_limit_mib,
        has_branched: false,
        last_branch_memory_path: None,
        branch_count: 0,
    };
    Ok((vm, info, netns_reservation, tap_claim))
}

async fn list_workspaces(State(s): State<SharedState>) -> Response {
    let v = s.registry.list_workspaces();
    Json(v).into_response()
}

async fn get_workspace(State(s): State<SharedState>, Path(name): Path<String>) -> Response {
    match s.registry.get_workspace(&name) {
        Some(ws) => Json(ws).into_response(),
        None => not_found(&format!("workspace {name}")),
    }
}

async fn create_workspace(
    State(s): State<SharedState>,
    Json(req): Json<CreateWorkspaceRequest>,
) -> Response {
    if !is_safe_tag(&req.name) {
        return bad_request("workspace name must be 1-64 chars, ASCII alnum or dash/underscore");
    }
    if !is_safe_tag(&req.snapshot_tag) {
        return bad_request("snapshot_tag must be 1-64 chars, ASCII alnum or dash/underscore");
    }
    if s.registry.get_workspace(&req.name).is_some() {
        return conflict(&format!(
            "workspace {} already exists; DELETE first",
            req.name
        ));
    }
    let snapshot_tag = req.snapshot_tag.clone();
    let per_child_netns = req.per_child_netns;
    let memory_limit_mib = req.memory_limit_mib;
    let s_clone = s.clone();
    let spawn_result = tokio::task::spawn_blocking(move || {
        spawn_one_for_workspace(&s_clone, &snapshot_tag, per_child_netns, memory_limit_mib)
    })
    .await;

    let (vm, sb_info, mut netns_reservation, mut tap_claim) = match spawn_result {
        Ok(Ok(tuple)) => tuple,
        Ok(Err(e)) if e.downcast_ref::<NetnsExhausted>().is_some() => {
            return service_unavailable(&format!("{e:#}"));
        }
        Ok(Err(e)) if e.downcast_ref::<SharedTapBusy>().is_some() => {
            return service_unavailable(&format!("{e:#}"));
        }
        Ok(Err(e)) => return server_error(&format!("spawn workspace sandbox: {e:#}")),
        Err(e) => return server_error(&format!("blocking task panicked: {e}")),
    };

    let id = sb_info.id.clone();
    if let Err(e) = s.registry.insert_sandbox(sb_info.clone()) {
        tracing::error!(error=%e, "persist workspace's live sandbox failed");
    }
    s.live_vms.lock().insert(id.clone(), vm);
    // Registration complete: commit both reservations AFTER live_vms.insert()
    // (security review #281 & #282). On spawn failure both are dropped and
    // released automatically. Committing after registration (not inside
    // spawn_blocking) ensures a cancelled handler between restore success
    // and registration does not permanently wedge the tap or leak netns
    // indices.
    if let Some(res) = netns_reservation.as_mut() {
        res.commit();
    }
    if let Some(claim) = tap_claim.as_mut() {
        claim.commit();
    }

    let now = unix_now();
    let ws = WorkspaceInfo {
        id: format!("ws-{}", &id[..id.len().min(16)]),
        name: req.name.clone(),
        source_snapshot_tag: req.snapshot_tag.clone(),
        current_state_tag: None,
        status: WorkspaceStatus::Running,
        live_sandbox_id: Some(id),
        created_at_unix: now,
        last_active_unix: now,
        last_branch_memory_path: None,
        per_child_netns,
    };
    if let Err(e) = s.registry.insert_workspace(ws.clone()) {
        return server_error(&format!("persist workspace: {e:#}"));
    }
    (StatusCode::CREATED, Json(ws)).into_response()
}

async fn delete_workspace(State(s): State<SharedState>, Path(name): Path<String>) -> Response {
    let ws = match s.registry.get_workspace(&name) {
        Some(w) => w,
        None => return not_found(&format!("workspace {name}")),
    };
    // Kill the live sandbox if any.
    if let Some(sb_id) = &ws.live_sandbox_id {
        if let Some(vm) = s.live_vms.lock().remove(sb_id) {
            // Kill firecracker first, then release the netns index and
            // shared-tap lease, closing the window where a new spawn
            // could enter forkd-child-N or grab forkd-tap0 while the
            // previous owner is still dying (review #281 & #282).
            let netns = vm.netns.clone();
            let is_shared_tap = netns.is_none();
            drop(vm); // Vm::drop kills firecracker + cleans cgroup
            release_netns_index(&s, netns.as_deref());
            release_shared_tap_if_owner(&s, sb_id, is_shared_tap);
        }
        let _ = s.registry.remove_sandbox(sb_id);
    }
    // Best-effort cleanup of the workspace's state snapshot. We DO
    // NOT remove the source snapshot — it might be shared with other
    // workspaces / sandboxes.
    if let Some(state_tag) = ws.current_state_tag.as_deref() {
        let dir = s.snapshot_root.join(state_tag);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = s.registry.remove_snapshot(state_tag);
    }
    let _ = s.registry.remove_workspace(&name);
    StatusCode::NO_CONTENT.into_response()
}

async fn suspend_workspace(
    State(s): State<SharedState>,
    Path(name): Path<String>,
    Json(req): Json<SuspendWorkspaceRequest>,
) -> Response {
    let ws = match s.registry.get_workspace(&name) {
        Some(w) => w,
        None => return not_found(&format!("workspace {name}")),
    };
    if ws.status != WorkspaceStatus::Running {
        return bad_request(&format!(
            "workspace {name} is {:?}, not Running — suspend requires a live sandbox",
            ws.status
        ));
    }
    let sb_id = match ws.live_sandbox_id.clone() {
        Some(id) => id,
        None => return server_error("inconsistent state: Running but no live_sandbox_id"),
    };

    // Pick a state-tag that we overwrite on each suspend; keeps disk
    // usage bounded at one snapshot per workspace.
    let state_tag = format!("ws-{name}-state");
    if !is_safe_tag(&state_tag) {
        return server_error("derived state tag failed validation (workspace name pathological?)");
    }
    let snap_dir = s.snapshot_root.join(&state_tag);

    // Hand-roll a slimmer branch path here. Acquire the slot via the
    // existing concurrency gate so we don't overlap with other branches.
    let _slot = match s.try_acquire_branch_slot(&state_tag) {
        Ok(slot) => slot,
        Err(BranchSlotError::AlreadyInFlight) => {
            return conflict(&format!("suspend for workspace '{name}' already in flight"));
        }
        Err(BranchSlotError::CapacityExceeded) => {
            return service_unavailable(&format!(
                "daemon at branch concurrency cap ({}); retry shortly",
                DEFAULT_BRANCH_CONCURRENCY
            ));
        }
    };

    // Delete any previous state snapshot so the new one can claim the dir.
    if snap_dir.join("vmstate").exists() {
        let _ = std::fs::remove_dir_all(&snap_dir);
        let _ = s.registry.remove_snapshot(&state_tag);
    }

    let vm = match s.live_vms.lock().remove(&sb_id) {
        Some(v) => v,
        None => return not_found(&format!("workspace's live sandbox {sb_id} is gone")),
    };
    // Wrap in VmNetnsGuard so the netns index is released even if the
    // handler is cancelled during the suspend task (review #282 r3).
    let vm = VmNetnsGuard::new(
        vm,
        s.netns_alloc.clone(),
        Some(s.shared_tap_owner.clone()),
        Some(sb_id.clone()),
    );
    let snap_dir_for_task = snap_dir.clone();
    let source_tag = ws.source_snapshot_tag.clone();
    let source_memory_path = s.snapshot_root.join(&source_tag).join("memory.bin");
    let last_chain = ws.last_branch_memory_path.clone();
    let diff_mode = req.diff;

    let task = tokio::task::spawn_blocking(move || -> (VmNetnsGuard, anyhow::Result<(forkd_vmm::Snapshot, Option<u64>)>) {
        let mut pause_ms: Option<u64> = None;
        let res = (|| -> anyhow::Result<forkd_vmm::Snapshot> {
            std::fs::create_dir_all(&snap_dir_for_task)?;
            let pause_start = std::time::Instant::now();
            let cp_handle: Option<std::thread::JoinHandle<std::io::Result<u64>>> = if diff_mode {
                let src = last_chain
                    .as_ref()
                    .filter(|p| p.exists())
                    .cloned()
                    .unwrap_or_else(|| source_memory_path.clone());
                let dst = snap_dir_for_task.join("memory.bin");
                Some(std::thread::spawn(move || std::fs::copy(&src, &dst)))
            } else {
                None
            };
            vm.pause()?;
            let snap = if diff_mode {
                let diff_path = std::env::temp_dir().join(format!(
                    "forkd-ws-diff-{}-{}.bin",
                    std::process::id(),
                    unix_now()
                ));
                let diff_snap = vm.snapshot_diff_to(
                    snap_dir_for_task.join("vmstate"),
                    diff_path.clone(),
                    Vec::new(),
                )?;
                vm.resume()?;
                pause_ms = Some(pause_start.elapsed().as_millis() as u64);
                if let Some(h) = cp_handle {
                    h.join()
                        .map_err(|e| anyhow::anyhow!("cp thread panicked: {e:?}"))??;
                }
                forkd_vmm::apply_diff(&diff_path, &snap_dir_for_task.join("memory.bin"))?;
                let _ = std::fs::remove_file(&diff_path);
                forkd_vmm::Snapshot {
                    vmstate: diff_snap.vmstate,
                    memory: snap_dir_for_task.join("memory.bin"),
                    volumes: diff_snap.volumes,
                    parent_tag: None,
                    parent_content_hash: None,
                    rootfs: None,
                    rootfs_read_only: None,
                }
            } else {
                let snap = vm.snapshot_to(
                    snap_dir_for_task.join("vmstate"),
                    snap_dir_for_task.join("memory.bin"),
                    Vec::new(),
                )?;
                vm.resume()?;
                pause_ms = Some(pause_start.elapsed().as_millis() as u64);
                snap
            };
            Ok(snap)
        })();
        (vm, res.map(|s| (s, pause_ms)))
    })
    .await;

    let (vm_guard, snap_or_err) = match task {
        Ok((vm, r)) => (vm, r),
        Err(e) => {
            // Blocking task panicked; the guard's Drop already ran during
            // the panic unwind, killing firecracker and releasing the
            // netns index. No further cleanup needed.
            return server_error(&format!("blocking task panicked: {e}"));
        }
    };

    // We took the VM out of live_vms for suspend; intentionally
    // discard it now (suspend == kill source after snapshotting).
    // The guard's Drop kills firecracker and releases the netns index
    // and shared-tap lease so the namespace/tap is vacated before they
    // become reusable (review #281 & #282). This also handles handler
    // cancellation: the guard is dropped when the task's return value
    // is dropped, ensuring both are always released (review #282 r3).
    drop(vm_guard);
    let _ = s.registry.remove_sandbox(&sb_id);

    let (snap, pause_ms) = match snap_or_err {
        Ok((s, p)) => (s, p),
        Err(e) => {
            let _ = std::fs::remove_dir_all(&snap_dir);
            return server_error(&format!("suspend: {e:#}"));
        }
    };

    // Persist snapshot.json so resume can find the volume / mem_file metadata.
    let meta = match serde_json::to_vec_pretty(&snap) {
        Ok(v) => v,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&snap_dir);
            return server_error(&format!("serialize snapshot.json: {e}"));
        }
    };
    if let Err(e) = std::fs::write(snap_dir.join("snapshot.json"), &meta) {
        let _ = std::fs::remove_dir_all(&snap_dir);
        return server_error(&format!("write snapshot.json: {e}"));
    }

    let snapshot_info = SnapshotInfo {
        tag: state_tag.clone(),
        dir: snap_dir.display().to_string(),
        created_at_unix: unix_now(),
        branched_from: Some(sb_id.clone()),
        pause_ms,
        diff_ms: None,
        diff_physical_bytes: None,
        diff_logical_bytes: None,
        warning: None,
        status: crate::api::SnapshotStatus::Ready,
        bootable: true,
    };
    if let Err(e) = s.registry.insert_snapshot(snapshot_info) {
        return server_error(&format!("persist suspend snapshot: {e:#}"));
    }

    let now = unix_now();
    if let Err(e) = s.registry.update_workspace(&name, |ws| {
        ws.status = WorkspaceStatus::Suspended;
        ws.live_sandbox_id = None;
        ws.current_state_tag = Some(state_tag.clone());
        ws.last_active_unix = now;
        ws.last_branch_memory_path = Some(snap_dir.join("memory.bin"));
    }) {
        return server_error(&format!("update workspace: {e:#}"));
    }

    let ws = match s.registry.get_workspace(&name) {
        Some(w) => w,
        None => return server_error("workspace vanished during suspend"),
    };
    Json(ws).into_response()
}

async fn resume_workspace(State(s): State<SharedState>, Path(name): Path<String>) -> Response {
    let ws = match s.registry.get_workspace(&name) {
        Some(w) => w,
        None => return not_found(&format!("workspace {name}")),
    };
    if ws.status == WorkspaceStatus::Running {
        return bad_request(&format!(
            "workspace {name} is already Running (sandbox {})",
            ws.live_sandbox_id.as_deref().unwrap_or("?")
        ));
    }
    // Pick the snapshot to spawn from: prefer current_state_tag (the
    // suspend snapshot), fall back to source if the workspace was
    // never suspended (Stale-from-startup case).
    let spawn_tag = ws
        .current_state_tag
        .clone()
        .unwrap_or_else(|| ws.source_snapshot_tag.clone());
    let s_clone = s.clone();
    // Respawn with the same netns layout the workspace was created
    // with (per_child_netns is persisted on WorkspaceInfo). Hardcoding
    // false here puts resumed sandboxes on the shared forkd-tap0,
    // which collides when several workspaces are active.
    let per_child_netns = ws.per_child_netns;
    let spawn_result = tokio::task::spawn_blocking(move || {
        spawn_one_for_workspace(&s_clone, &spawn_tag, per_child_netns, None)
    })
    .await;
    let (vm, sb_info, mut netns_reservation, mut tap_claim) = match spawn_result {
        Ok(Ok(tuple)) => tuple,
        Ok(Err(e)) if e.downcast_ref::<NetnsExhausted>().is_some() => {
            return service_unavailable(&format!("{e:#}"));
        }
        Ok(Err(e)) if e.downcast_ref::<SharedTapBusy>().is_some() => {
            return service_unavailable(&format!("{e:#}"));
        }
        Ok(Err(e)) => return server_error(&format!("spawn workspace sandbox: {e:#}")),
        Err(e) => return server_error(&format!("blocking task panicked: {e}")),
    };
    let id = sb_info.id.clone();
    if let Err(e) = s.registry.insert_sandbox(sb_info.clone()) {
        tracing::error!(error=%e, "persist workspace's live sandbox failed");
    }
    s.live_vms.lock().insert(id.clone(), vm);
    // Registration complete: commit both reservations AFTER live_vms.insert()
    // (security review #281 & #282). On spawn failure both are dropped and
    // released automatically.  This mirrors create_workspace — the
    // previous binding `_netns_reservation` (underscore = unused) dropped
    // the reservation uncommitted, releasing the indices while the
    // resumed VM was still live and re-opening the exact race #282 fixes
    // (review #282 round 2).
    if let Some(res) = netns_reservation.as_mut() {
        res.commit();
    }
    if let Some(claim) = tap_claim.as_mut() {
        claim.commit();
    }

    let now = unix_now();
    if let Err(e) = s.registry.update_workspace(&name, |w| {
        w.status = WorkspaceStatus::Running;
        w.live_sandbox_id = Some(id.clone());
        w.last_active_unix = now;
    }) {
        return server_error(&format!("update workspace: {e:#}"));
    }

    let ws = match s.registry.get_workspace(&name) {
        Some(w) => w,
        None => return server_error("workspace vanished during resume"),
    };
    Json(ws).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    /// `AppState` on a private scratch `TempDir` that the state itself owns,
    /// so the directory is reaped when the last `Arc` goes away. Every path
    /// the daemon writes to — registry file, `snapshot_root`, prewarm
    /// scratch — lives inside it, so a test run leaves nothing behind in
    /// `$TMPDIR` (issue #273).
    fn test_state_with_cap(cap: usize) -> SharedState {
        let td = tempfile::TempDir::new().unwrap();
        let path = td.path().join("state.json");
        let snapshot_root = td.path().join("snapshots");
        let prewarm_scratch_dir = td.path().join("prewarm");
        Arc::new(AppState {
            registry: Registry::load_or_init(path).unwrap(),
            live_vms: Mutex::new(HashMap::new()),
            snapshot_root,
            branch_in_flight: Mutex::new(HashSet::new()),
            branch_sem: Arc::new(Semaphore::new(cap)),
            branch_concurrency_cap: cap,
            netns_alloc: crate::netns::NetnsAllocator::new(
                256,
                Box::new(crate::netns::RangeProbe { max: 256 }),
            ),
            shared_tap_owner: std::sync::Arc::new(parking_lot::Mutex::new(None)),
            orphan_firecrackers_detected: std::sync::atomic::AtomicU64::new(0),
            prewarm_scratch_dir,
            #[cfg(target_os = "linux")]
            live_in_flight: Mutex::new(HashMap::new()),
            _tempdir: Some(td),
        })
    }

    fn test_state() -> SharedState {
        test_state_with_cap(DEFAULT_BRANCH_CONCURRENCY)
    }

    /// test_state with a netns pool sized `netns_pool` (0 = exhausted).
    fn test_state_with_netns(netns_pool: usize) -> SharedState {
        let td = tempfile::TempDir::new().unwrap();
        let path = td.path().join("state.json");
        let snapshot_root = td.path().join("snapshots");
        let prewarm_scratch_dir = td.path().join("prewarm");
        Arc::new(AppState {
            registry: Registry::load_or_init(path).unwrap(),
            live_vms: Mutex::new(HashMap::new()),
            snapshot_root,
            branch_in_flight: Mutex::new(HashSet::new()),
            branch_sem: Arc::new(Semaphore::new(DEFAULT_BRANCH_CONCURRENCY)),
            branch_concurrency_cap: DEFAULT_BRANCH_CONCURRENCY,
            netns_alloc: crate::netns::NetnsAllocator::new(
                netns_pool,
                Box::new(crate::netns::RangeProbe { max: netns_pool }),
            ),
            shared_tap_owner: std::sync::Arc::new(parking_lot::Mutex::new(None)),
            orphan_firecrackers_detected: std::sync::atomic::AtomicU64::new(0),
            prewarm_scratch_dir,
            #[cfg(target_os = "linux")]
            live_in_flight: Mutex::new(HashMap::new()),
            _tempdir: Some(td),
        })
    }

    #[test]
    fn new_sandbox_id_uses_fixed_width_counter_suffix() {
        let id = new_sandbox_id();
        let mut parts = id.split('-');
        assert_eq!(parts.next(), Some("sb"));
        let ts = parts.next().expect("timestamp segment");
        let suffix = parts.next().expect("counter segment");
        assert!(parts.next().is_none(), "unexpected extra id segment: {id}");
        assert!(!ts.is_empty(), "timestamp segment must not be empty: {id}");
        assert_eq!(suffix.len(), 16, "counter suffix must be fixed width: {id}");
        assert!(
            suffix.chars().all(|c| c.is_ascii_hexdigit()),
            "counter suffix must be hex: {id}"
        );
    }

    /// Write a base snapshot directory on disk under `state.snapshot_root`.
    /// Just enough plumbing for the chain-aware load helpers to find it;
    /// the memory.bin / vmstate files exist but aren't FC-valid.
    fn write_base_snapshot(state: &SharedState, tag: &str) -> PathBuf {
        let dir = state.snapshot_root.join(tag);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("vmstate"), b"fake-vmstate").unwrap();
        std::fs::write(dir.join("memory.bin"), b"fake-memory").unwrap();
        let snap = forkd_vmm::Snapshot {
            vmstate: dir.join("vmstate"),
            memory: dir.join("memory.bin"),
            volumes: Vec::new(),
            parent_tag: None,
            parent_content_hash: None,
            rootfs: None,
            rootfs_read_only: None,
        };
        std::fs::write(
            dir.join("snapshot.json"),
            serde_json::to_vec_pretty(&snap).unwrap(),
        )
        .unwrap();
        dir
    }

    /// Write a chained snapshot directory referencing `parent_tag`.
    fn write_chained_snapshot(
        state: &SharedState,
        tag: &str,
        parent_tag: &str,
        parent_hash: &str,
    ) -> PathBuf {
        let dir = state.snapshot_root.join(tag);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("vmstate"), b"fake-vmstate").unwrap();
        std::fs::write(dir.join("diff.bin"), b"fake-diff").unwrap();
        let snap = forkd_vmm::Snapshot {
            vmstate: dir.join("vmstate"),
            memory: dir.join("diff.bin"),
            volumes: Vec::new(),
            parent_tag: Some(parent_tag.to_string()),
            parent_content_hash: Some(parent_hash.to_string()),
            rootfs: None,
            rootfs_read_only: None,
        };
        std::fs::write(
            dir.join("snapshot.json"),
            serde_json::to_vec_pretty(&snap).unwrap(),
        )
        .unwrap();
        dir
    }

    // ------------------------------------------------------------
    // Phase 2a — chain-aware snapshot-load helpers.
    // ------------------------------------------------------------

    #[test]
    fn load_snapshot_with_fallback_missing_dir_returns_base() {
        // Dir doesn't exist on disk. Helper must still return a
        // base-shaped Snapshot pointing at the expected fallback
        // paths (this is what v0.4-and-earlier flows depend on).
        let nonexistent =
            std::env::temp_dir().join(format!("load-fallback-missing-{}", std::process::id()));
        let snap = load_snapshot_with_fallback(&nonexistent);
        assert!(snap.parent_tag.is_none());
        assert_eq!(snap.memory, nonexistent.join("memory.bin"));
    }

    #[test]
    fn load_snapshot_with_fallback_parses_chained_json() {
        let state = test_state();
        let dir = write_chained_snapshot(&state, "py-plus-pandas", "py-base", "h-pin");
        let snap = load_snapshot_with_fallback(&dir);
        assert_eq!(snap.parent_tag.as_deref(), Some("py-base"));
        assert_eq!(snap.parent_content_hash.as_deref(), Some("h-pin"));
        assert!(snap.memory.ends_with("diff.bin"));
    }

    #[test]
    fn resolve_snap_dir_falls_back_to_disk_when_not_registered() {
        let state = test_state();
        let dir = write_base_snapshot(&state, "py-base");
        let resolved = resolve_snap_dir(&state.registry, &state.snapshot_root, "py-base").unwrap();
        assert_eq!(resolved, dir);
    }

    #[test]
    fn resolve_snap_dir_errors_on_unknown_tag() {
        let state = test_state();
        let err = resolve_snap_dir(&state.registry, &state.snapshot_root, "ghost").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("ghost"),
            "error must name the missing tag: {msg}"
        );
    }

    #[test]
    fn lookup_snapshot_for_chain_threads_through_to_load() {
        // Round-trip the closure used by resolve_chain in spawn handler.
        let state = test_state();
        write_base_snapshot(&state, "py-base");
        write_chained_snapshot(&state, "py-plus-pandas", "py-base", "h-pin");

        let base =
            lookup_snapshot_for_chain(&state.registry, &state.snapshot_root, "py-base").unwrap();
        assert!(base.parent_tag.is_none());
        let chained =
            lookup_snapshot_for_chain(&state.registry, &state.snapshot_root, "py-plus-pandas")
                .unwrap();
        assert_eq!(chained.parent_tag.as_deref(), Some("py-base"));
    }

    // ------------------------------------------------------------
    // Phase 2b — BRANCH-with-parent_tag validation.
    // ------------------------------------------------------------

    /// Helper: register a sandbox in the test state so branch_sandbox
    /// can find it via `s.registry.get_sandbox`. Returns the sandbox id.
    /// The sandbox isn't actually backed by a live FC process — these
    /// tests exercise the request-validation early returns *before*
    /// the spawn_blocking restore work, so a registry-only sandbox is
    /// enough.
    fn register_fake_sandbox(state: &SharedState, snapshot_tag: &str) -> String {
        let id = format!("sb-test-{}", unix_now());
        let info = SandboxInfo {
            id: id.clone(),
            snapshot_tag: snapshot_tag.to_string(),
            netns: None,
            guest_addr: "10.42.0.2:8888".to_string(),
            created_at_unix: unix_now(),
            pid: None,
            proc_starttime: None,
            boot_id: None,
            memory_limit_mib: None,
            has_branched: false,
            last_branch_memory_path: None,
            branch_count: 0,
        };
        state.registry.insert_sandbox(info).unwrap();
        id
    }

    #[tokio::test]
    async fn branch_parent_tag_mismatch_returns_400() {
        // parent_tag must equal the source sandbox's snapshot_tag.
        // Catches the "user typo / chain-edge-pointing-at-wrong-base"
        // foot-gun before the daemon writes a snapshot.json with a
        // chain edge that wouldn't restore.
        let state = test_state();
        write_base_snapshot(&state, "py-base");
        let sandbox_id = register_fake_sandbox(&state, "py-base");

        let app = router(state.clone());
        let body = serde_json::json!({
            "tag": "py-plus-pandas",
            "diff": true,
            "parent_tag": "some-other-base",  // ← mismatch
        });
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/v1/sandboxes/{sandbox_id}/branch"))
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(resp.into_body(), 4096).await.unwrap();
        let s = std::str::from_utf8(&body).unwrap();
        assert!(
            s.contains("some-other-base") && s.contains("py-base"),
            "error must name both expected and actual: {s}"
        );
    }

    #[tokio::test]
    async fn branch_parent_tag_with_full_mode_returns_400() {
        // parent_tag requires diff mode. Setting it with the default
        // (Full) BRANCH writes a non-chained snapshot, which would
        // record a chain edge that doesn't match the actual memory
        // shape — explicit reject.
        let state = test_state();
        write_base_snapshot(&state, "py-base");
        let sandbox_id = register_fake_sandbox(&state, "py-base");

        let app = router(state.clone());
        let body = serde_json::json!({
            "tag": "py-plus-pandas",
            // No `diff: true`, no `mode: "diff"` — Full BRANCH.
            "parent_tag": "py-base",
        });
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/v1/sandboxes/{sandbox_id}/branch"))
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(resp.into_body(), 4096).await.unwrap();
        let s = std::str::from_utf8(&body).unwrap();
        assert!(
            s.contains("parent_tag") && s.contains("diff"),
            "error must mention parent_tag and diff: {s}"
        );
    }

    #[tokio::test]
    async fn spawn_chained_with_live_fork_returns_400() {
        // The documented v0.5 carve-out: live_fork=true on a chained
        // snapshot needs additional memfd-population plumbing that
        // ships in v0.6 per DESIGN-v0.5 "Live-fork interaction".
        // v0.5 returns HTTP 400 so callers get a clear "not yet."
        let state = test_state();
        // Real sha256 of the base bytes so the carve-out check fires
        // BEFORE verify_parent_hashes would have caught a wrong hash
        // (we're testing the carve-out path, not the hash check).
        let base_dir = write_base_snapshot(&state, "py-base");
        let base_hash = forkd_vmm::chain::sha256_file(&base_dir.join("memory.bin")).unwrap();
        write_chained_snapshot(&state, "py-plus-pandas", "py-base", &base_hash);

        let app = router(state.clone());
        let body = serde_json::json!({
            "snapshot_tag": "py-plus-pandas",
            "n": 1,
            "per_child_netns": false,
            "live_fork": true,
        });
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/sandboxes")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(resp.into_body(), 4096).await.unwrap();
        let s = std::str::from_utf8(&body).unwrap();
        assert!(
            s.contains("live_fork") && s.contains("chained"),
            "error must mention the carve-out: {s}"
        );
    }

    // ------------------------------------------------------------
    // v0.5 Phase 4 — chain-aware rmi + info + compact.
    // ------------------------------------------------------------

    #[tokio::test]
    async fn snapshot_info_reports_chain_depth_and_dependents() {
        let state = test_state();
        let base_dir = write_base_snapshot(&state, "py-base");
        let base_hash = forkd_vmm::chain::sha256_file(&base_dir.join("memory.bin")).unwrap();
        write_chained_snapshot(&state, "py-numpy", "py-base", &base_hash);
        // grandchild — but write a real hash chained off py-numpy's "memory"
        // file (the fake diff.bin), so verify_parent_hashes downstream
        // wouldn't choke if anyone uses this fixture for restore tests.
        let py_numpy_dir = state.snapshot_root.join("py-numpy");
        let py_numpy_hash = forkd_vmm::chain::sha256_file(&py_numpy_dir.join("diff.bin")).unwrap();
        write_chained_snapshot(&state, "py-pandas", "py-numpy", &py_numpy_hash);

        let app = router(state.clone());

        // py-base has 2 transitive descendants but only 1 direct
        // dependent (py-numpy). depth 0.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/snapshots/py-base/info")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), 8192).await.unwrap();
        let detail: SnapshotInfoDetail = serde_json::from_slice(&body).unwrap();
        assert_eq!(detail.tag, "py-base");
        assert_eq!(detail.parent_tag, None);
        assert_eq!(detail.chain_depth, 0);
        assert_eq!(detail.ancestors, Vec::<String>::new());
        assert_eq!(detail.dependents, vec!["py-numpy".to_string()]);

        // py-numpy has 1 ancestor (py-base) and 1 dependent (py-pandas).
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/snapshots/py-numpy/info")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), 8192).await.unwrap();
        let detail: SnapshotInfoDetail = serde_json::from_slice(&body).unwrap();
        assert_eq!(detail.chain_depth, 1);
        assert_eq!(detail.ancestors, vec!["py-base".to_string()]);
        assert_eq!(detail.dependents, vec!["py-pandas".to_string()]);

        // py-pandas: leaf. depth 2, no dependents.
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/v1/snapshots/py-pandas/info")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), 8192).await.unwrap();
        let detail: SnapshotInfoDetail = serde_json::from_slice(&body).unwrap();
        assert_eq!(detail.chain_depth, 2);
        assert_eq!(
            detail.ancestors,
            vec!["py-base".to_string(), "py-numpy".to_string()]
        );
        assert!(detail.dependents.is_empty());
    }

    #[tokio::test]
    async fn snapshot_info_missing_returns_404() {
        let app = router(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/v1/snapshots/does-not-exist/info")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn delete_snapshot_with_dependents_returns_409() {
        let state = test_state();
        let base_dir = write_base_snapshot(&state, "py-base");
        let base_hash = forkd_vmm::chain::sha256_file(&base_dir.join("memory.bin")).unwrap();
        write_chained_snapshot(&state, "py-numpy", "py-base", &base_hash);

        let app = router(state.clone());
        let resp = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/v1/snapshots/py-base")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let body = to_bytes(resp.into_body(), 4096).await.unwrap();
        let s = std::str::from_utf8(&body).unwrap();
        assert!(
            s.contains("py-numpy") && s.contains("cascade") && s.contains("force"),
            "409 body must list children + name both escape hatches: {s}"
        );

        // Both files must still be on disk + registry — refusal mustn't
        // delete anything.
        assert!(state.snapshot_root.join("py-base").exists());
        assert!(state.snapshot_root.join("py-numpy").exists());
    }

    #[tokio::test]
    async fn delete_snapshot_cascade_deletes_subtree() {
        let state = test_state();
        let base_dir = write_base_snapshot(&state, "py-base");
        let base_hash = forkd_vmm::chain::sha256_file(&base_dir.join("memory.bin")).unwrap();
        write_chained_snapshot(&state, "py-numpy", "py-base", &base_hash);
        let numpy_dir = state.snapshot_root.join("py-numpy");
        let numpy_hash = forkd_vmm::chain::sha256_file(&numpy_dir.join("diff.bin")).unwrap();
        write_chained_snapshot(&state, "py-pandas", "py-numpy", &numpy_hash);

        let app = router(state.clone());
        let resp = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/v1/snapshots/py-base?cascade=true")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        // Whole subtree gone.
        assert!(!state.snapshot_root.join("py-base").exists());
        assert!(!state.snapshot_root.join("py-numpy").exists());
        assert!(!state.snapshot_root.join("py-pandas").exists());
    }

    #[tokio::test]
    async fn delete_snapshot_force_orphans_children() {
        let state = test_state();
        let base_dir = write_base_snapshot(&state, "py-base");
        let base_hash = forkd_vmm::chain::sha256_file(&base_dir.join("memory.bin")).unwrap();
        write_chained_snapshot(&state, "py-numpy", "py-base", &base_hash);

        let app = router(state.clone());
        let resp = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/v1/snapshots/py-base?force=true")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        // Parent gone, child still on disk (orphaned).
        assert!(!state.snapshot_root.join("py-base").exists());
        assert!(state.snapshot_root.join("py-numpy").exists());
    }

    #[tokio::test]
    async fn delete_snapshot_cascade_and_force_together_returns_400() {
        let state = test_state();
        write_base_snapshot(&state, "py-base");
        let app = router(state.clone());
        let resp = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/v1/snapshots/py-base?cascade=true&force=true")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn compact_snapshot_rejects_existing_target_tag() {
        let state = test_state();
        write_base_snapshot(&state, "py-base");
        write_base_snapshot(&state, "py-base-flat"); // squat the target
        let app = router(state.clone());
        let body = serde_json::json!({ "to": "py-base-flat" });
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/snapshots/py-base/compact")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn compact_snapshot_same_from_and_to_returns_400() {
        let state = test_state();
        write_base_snapshot(&state, "py-base");
        let app = router(state.clone());
        let body = serde_json::json!({ "to": "py-base" });
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/snapshots/py-base/compact")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn healthz_returns_ok() {
        let app = router(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn version_has_build_info() {
        let app = router(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/version")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), 1024).await.unwrap();
        let v: VersionResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(v.api, "v1");
    }

    #[tokio::test]
    async fn metrics_emits_prometheus_text() {
        let app = router(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), 4096).await.unwrap();
        let s = std::str::from_utf8(&body).unwrap();
        assert!(s.contains("forkd_sandboxes_active 0"));
        assert!(s.contains("forkd_build_info"));
        // BRANCH concurrency observability (see #177 / #179 follow-up).
        assert!(s.contains("forkd_branches_in_flight 0"));
        assert!(
            s.contains(&format!(
                "forkd_branch_concurrency_cap {DEFAULT_BRANCH_CONCURRENCY}"
            )),
            "expected cap to surface as the test_state default; got body:\n{s}"
        );
    }

    #[tokio::test]
    async fn metrics_branches_in_flight_tracks_slot_acquisitions() {
        // Regression for the #179 follow-up: forkd_branches_in_flight
        // must increment while a BranchSlot is held and decrement when
        // it's dropped. Without this guarantee, operators can't size
        // FORKD_BRANCH_CONCURRENCY empirically — which is the whole
        // point of exposing it as a CLI flag.
        let state = test_state();
        let slot_a = state.try_acquire_branch_slot("t1").unwrap();
        let slot_b = state.try_acquire_branch_slot("t2").unwrap();
        let app = router(state.clone());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = to_bytes(resp.into_body(), 4096).await.unwrap();
        let s = std::str::from_utf8(&body).unwrap();
        assert!(
            s.contains("forkd_branches_in_flight 2"),
            "expected 2 in-flight branches while two slots are held; got:\n{s}"
        );
        // Drop both — the gauge must come back to 0.
        drop(slot_a);
        drop(slot_b);
        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = to_bytes(resp.into_body(), 4096).await.unwrap();
        let s = std::str::from_utf8(&body).unwrap();
        assert!(
            s.contains("forkd_branches_in_flight 0"),
            "expected gauge to return to 0 after slot drops; got:\n{s}"
        );
    }

    #[tokio::test]
    async fn empty_sandbox_list() {
        let app = router(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/v1/sandboxes")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), 1024).await.unwrap();
        let list: Vec<crate::api::SandboxInfo> = serde_json::from_slice(&body).unwrap();
        assert!(list.is_empty());
    }

    #[tokio::test]
    async fn missing_sandbox_returns_404() {
        let app = router(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/v1/sandboxes/does-not-exist")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn create_snapshot_rejects_unsafe_tag() {
        let app = router(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/snapshots")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"tag":"../etc/passwd","kernel":"/dev/null","rootfs":"/dev/null"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn branch_slot_same_tag_serialises() {
        let s = test_state();
        let a = s
            .try_acquire_branch_slot("foo")
            .expect("first acquire should succeed");
        // Same tag, while first slot is alive → 409 condition.
        let err = s
            .try_acquire_branch_slot("foo")
            .expect_err("second acquire on same tag should fail");
        assert_eq!(err, BranchSlotError::AlreadyInFlight);
        drop(a);
        // After release, same tag must be acquirable again.
        let _b = s
            .try_acquire_branch_slot("foo")
            .expect("re-acquire after drop should succeed");
    }

    #[test]
    fn branch_slot_different_tags_parallel() {
        let s = test_state();
        let _a = s.try_acquire_branch_slot("foo").unwrap();
        let _b = s
            .try_acquire_branch_slot("bar")
            .expect("different tag should not collide");
    }

    #[test]
    fn branch_slot_global_cap_blocks() {
        // Cap = 2 so the test stays deterministic. Reaches the 503 path.
        let s = test_state_with_cap(2);
        let _a = s.try_acquire_branch_slot("t1").unwrap();
        let _b = s.try_acquire_branch_slot("t2").unwrap();
        let err = s
            .try_acquire_branch_slot("t3")
            .expect_err("third slot should be refused");
        assert_eq!(err, BranchSlotError::CapacityExceeded);
    }

    #[test]
    fn branch_slot_capacity_recovers_on_drop() {
        let s = test_state_with_cap(1);
        let a = s.try_acquire_branch_slot("t1").unwrap();
        assert!(s.try_acquire_branch_slot("t2").is_err());
        drop(a);
        let _b = s
            .try_acquire_branch_slot("t2")
            .expect("slot should free up after Drop");
    }

    #[test]
    fn branch_suffix_changes_between_calls() {
        let a = branch_suffix();
        let b = branch_suffix();
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn create_snapshot_rejects_missing_kernel() {
        let app = router(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/snapshots")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"tag":"ok","kernel":"/nonexistent","rootfs":"/dev/null"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn delete_snapshot_missing_returns_404() {
        let app = router(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/v1/snapshots/does-not-exist")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn create_sandbox_missing_snapshot_returns_404() {
        let app = router(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/sandboxes")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"snapshot_tag":"does-not-exist","n":1}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn create_sandbox_netns_pool_exhaustion_returns_503() {
        // A zero-provisioned allocator must reject the spawn with a
        // resource-exhaustion 503 (not a generic 500) — security review
        // #282.
        let state = test_state_with_netns(0);
        let dir = state.snapshot_root.join("base");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("vmstate"), b"fake-vmstate").unwrap();
        std::fs::write(dir.join("memory.bin"), b"fake-memory").unwrap();

        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/sandboxes")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"snapshot_tag":"base","n":1,"per_child_netns":true}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "netns exhaustion must be 503, not a generic 500"
        );
    }

    #[tokio::test]
    async fn create_workspace_netns_exhaustion_returns_503() {
        // Workspace create must map netns exhaustion to 503, not 500,
        // consistent with the sandbox path (review #282 round 2).
        let state = test_state_with_netns(0);
        let dir = state.snapshot_root.join("base");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("vmstate"), b"fake-vmstate").unwrap();
        std::fs::write(dir.join("memory.bin"), b"fake-memory").unwrap();

        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/workspaces")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"name":"ws-test","snapshot_tag":"base","per_child_netns":true}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "workspace netns exhaustion must be 503, not a generic 500"
        );
    }

    #[tokio::test]
    async fn resume_workspace_netns_exhaustion_returns_503() {
        // Resume must also map netns exhaustion to 503, not 500
        // (review #282 round 2). We set up a suspended workspace
        // (no live sandbox) with a state snapshot to resume from.
        let state = test_state_with_netns(0);
        let state_dir = state.snapshot_root.join("ws-test-state");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(state_dir.join("vmstate"), b"fake-vmstate").unwrap();
        std::fs::write(state_dir.join("memory.bin"), b"fake-memory").unwrap();
        let snap = forkd_vmm::Snapshot {
            vmstate: state_dir.join("vmstate"),
            memory: state_dir.join("memory.bin"),
            volumes: Vec::new(),
            parent_tag: None,
            parent_content_hash: None,
            rootfs: None,
            rootfs_read_only: None,
        };
        std::fs::write(
            state_dir.join("snapshot.json"),
            serde_json::to_vec_pretty(&snap).unwrap(),
        )
        .unwrap();
        let snap_info = crate::api::SnapshotInfo {
            tag: "ws-test-state".into(),
            dir: state_dir.display().to_string(),
            created_at_unix: 1,
            branched_from: None,
            pause_ms: None,
            diff_ms: None,
            diff_physical_bytes: None,
            diff_logical_bytes: None,
            warning: None,
            status: crate::api::SnapshotStatus::Ready,
            bootable: true,
        };
        state.registry.insert_snapshot(snap_info).unwrap();
        // Insert a Suspended workspace so resume is valid.
        let ws = WorkspaceInfo {
            id: "ws-test".into(),
            name: "ws-test".into(),
            source_snapshot_tag: "base".into(),
            current_state_tag: Some("ws-test-state".into()),
            status: WorkspaceStatus::Suspended,
            live_sandbox_id: None,
            created_at_unix: 1,
            last_active_unix: 1,
            last_branch_memory_path: Some(state_dir.join("memory.bin")),
            per_child_netns: true,
        };
        state.registry.insert_workspace(ws).unwrap();

        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/workspaces/ws-test/resume")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "resume workspace netns exhaustion must be 503, not a generic 500"
        );
    }

    /// Review #281: while a shared-tap VM is live, a second shared-tap
    /// spawn must be rejected with 503 (not retried into an EBUSY
    /// loop). The lease claim happens inside the spawn_blocking task,
    /// before restore, so no firecracker is needed to exercise it.
    #[tokio::test]
    async fn create_sandbox_rejects_second_shared_tap_owner_with_503() {
        let state = test_state();
        write_base_snapshot(&state, "base");
        // Simulate an already-live shared-tap VM: the owner lease is held.
        *state.shared_tap_owner.lock() = Some("sb-live-1".to_string());

        let app = router(state.clone());
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/sandboxes")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"snapshot_tag":"base","n":1,"per_child_netns":false}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = to_bytes(resp.into_body(), 4096).await.unwrap();
        let err: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            err["error"].to_string().contains("forkd-tap0"),
            "503 should name the shared tap, got: {}",
            err["error"]
        );
        // Lease must still be held by the original owner.
        assert_eq!(state.shared_tap_owner.lock().as_deref(), Some("sb-live-1"));
    }

    /// Review #281: per_child_netns=true spawns bypass the shared-tap
    /// lease entirely (each netns has its own tap), so a per-netns
    /// request must NOT be rejected when the shared tap is owned.
    #[tokio::test]
    async fn create_sandbox_per_netns_ignores_shared_tap_lease() {
        let state = test_state();
        write_base_snapshot(&state, "base");
        *state.shared_tap_owner.lock() = Some("sb-live-1".to_string());

        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/sandboxes")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"snapshot_tag":"base","n":1,"per_child_netns":true}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_ne!(
            resp.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "per-netns spawn must not hit the shared-tap 503 path"
        );
    }

    /// Review #281: lease lifetime semantics — claim blocks a second
    /// claimer, uncommitted drop (spawn failure) releases, commit
    /// (successful spawn) keeps the lease until explicit VM teardown.
    #[test]
    fn shared_tap_lease_claim_release_and_drop_semantics() {
        let cell = std::sync::Arc::new(parking_lot::Mutex::new(None::<String>));

        let c1 = try_claim_shared_tap(&cell, "a").expect("first claim wins");
        assert!(
            try_claim_shared_tap(&cell, "b").is_none(),
            "second claim must fail"
        );
        assert_eq!(cell.lock().as_deref(), Some("a"));

        // Uncommitted drop (spawn failure path) releases the lease.
        drop(c1);
        assert!(cell.lock().is_none(), "drop must release");

        let mut c2 = try_claim_shared_tap(&cell, "c").expect("reclaim after drop");
        assert_eq!(cell.lock().as_deref(), Some("c"));
        // Commit (successful spawn) keeps the lease alive past drop.
        c2.commit();
        drop(c2);
        assert_eq!(
            cell.lock().as_deref(),
            Some("c"),
            "committed lease survives guard drop"
        );

        // Explicit teardown clears the owner cell.
        *cell.lock() = None;
        assert!(
            try_claim_shared_tap(&cell, "d").is_some(),
            "reclaim after teardown"
        );
    }

    #[tokio::test]
    async fn list_snapshots_marks_rootfs_only_snapshot_unbootable() {
        let state = test_state();
        let dir = state.snapshot_root.join("rootfs-only");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("vmstate"), b"fake-vmstate").unwrap();
        std::fs::write(dir.join("rootfs.ext4"), b"fake-rootfs").unwrap();

        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/v1/snapshots")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), 4096).await.unwrap();
        let snapshots: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        let snap = snapshots
            .iter()
            .find(|s| s["tag"] == "rootfs-only")
            .expect("rootfs-only tag should be listed");
        assert_eq!(snap["status"], "failed");
        assert_eq!(snap["bootable"], false);
        let warning = snap["warning"].as_str().unwrap();
        assert!(
            warning.contains("memory") && warning.contains("memory.bin"),
            "warning must name the missing memory artifact: {warning}"
        );
    }

    #[tokio::test]
    async fn create_sandbox_unbootable_snapshot_names_missing_memory() {
        let state = test_state();
        let dir = state.snapshot_root.join("rootfs-only");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("vmstate"), b"fake-vmstate").unwrap();
        std::fs::write(dir.join("rootfs.ext4"), b"fake-rootfs").unwrap();

        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/sandboxes")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"snapshot_tag":"rootfs-only","n":1}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let body = to_bytes(resp.into_body(), 4096).await.unwrap();
        let s = std::str::from_utf8(&body).unwrap();
        assert!(
            s.contains("rootfs-only") && s.contains("memory.bin"),
            "error must name the tag and missing memory artifact: {s}"
        );
    }

    #[tokio::test]
    async fn create_sandbox_rejects_unsafe_snapshot_tag_traversal() {
        // Regression: `create_sandbox` previously skipped `is_safe_tag` on the
        // request body's `snapshot_tag`. A traversing value like
        // `../../etc/passwd` would fall through to the disk-fallback branch
        // where `snapshot_root.join(tag)` produces a path that std::fs syscalls
        // resolve outside snapshot_root. The 404-from-vmstate-existence-check
        // partially limited impact, but the unvalidated tag also got persisted
        // into SandboxInfo.snapshot_tag and later flowed into
        // `read_snapshot_volumes`, where it would parse attacker-chosen JSON
        // files as forkd_vmm::Snapshot and inherit their volumes into branches.
        //
        // Expect 400 (input validation), not 404 (file-existence oracle).
        let app = router(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/sandboxes")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"snapshot_tag":"../../etc/passwd","n":1}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn create_snapshot_rejects_boot_wait_over_cap() {
        // Regression: `boot_wait_secs` was untyped u64 with no cap, so a
        // hostile caller could pass u64::MAX to tie up a daemon worker.
        let app = router(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/snapshots")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"tag":"ok","kernel":"/dev/null","rootfs":"/dev/null","boot_wait_secs":999999}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn create_sandbox_rejects_unsafe_snapshot_tag_chars() {
        // Defense in depth: also reject tags containing characters that aren't
        // ASCII alnum / dash / underscore (matches `is_safe_tag`'s contract).
        let app = router(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/sandboxes")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"snapshot_tag":"tag with space","n":1}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn create_sandbox_rejects_zero_n() {
        let app = router(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/sandboxes")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"snapshot_tag":"x","n":0}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn ping_missing_sandbox_returns_404() {
        let app = router(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/sandboxes/missing/ping")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn exec_empty_args_returns_400() {
        // Register a fake sandbox first so we get past the 404 check.
        let s = test_state();
        s.registry
            .insert_sandbox(SandboxInfo {
                id: "sb-fake".into(),
                snapshot_tag: "x".into(),
                netns: None,
                guest_addr: "127.0.0.1:1".into(),
                created_at_unix: 1,
                pid: Some(99999999),
                proc_starttime: None,
                boot_id: None,
                memory_limit_mib: None,
                has_branched: false,
                last_branch_memory_path: None,
                branch_count: 0,
            })
            .unwrap();
        let app = router(s);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/sandboxes/sb-fake/exec")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"args":[]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn delete_sandbox_missing_returns_404() {
        let app = router(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/v1/sandboxes/nope")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn branch_missing_sandbox_returns_404() {
        let app = router(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/sandboxes/nope/branch")
                    .header("Content-Type", "application/json")
                    .body(Body::from(r#"{}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn branch_rejects_unsafe_tag() {
        let app = router(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/sandboxes/anything/branch")
                    .header("Content-Type", "application/json")
                    .body(Body::from(r#"{"tag":"../etc/passwd"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn branch_rejects_mode_plus_legacy_bools() {
        // Phase 7: `mode` is canonical; can't combine it with legacy
        // `diff` / `live` even if the resolved mode would match
        // (preserves a single source of truth).
        for body in [
            r#"{"mode":"live","live":true}"#,
            r#"{"mode":"diff","diff":true}"#,
            r#"{"mode":"full","diff":false,"live":true}"#,
            r#"{"mode":"diff","live":true}"#,
        ] {
            let app = router(test_state());
            let resp = app
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/v1/sandboxes/anything/branch")
                        .header("Content-Type", "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::BAD_REQUEST,
                "expected 400 for body: {body}",
            );
        }
    }

    #[tokio::test]
    async fn branch_mode_full_for_missing_sandbox_returns_404_not_400() {
        // `mode: "full"` alone with no legacy bools should validate
        // and proceed past the request-validation phase; the next
        // failure point (missing sandbox) returns 404. If we
        // accidentally regressed and the mode resolution rejected
        // valid input, we'd see 400 here instead.
        let app = router(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/sandboxes/nope/branch")
                    .header("Content-Type", "application/json")
                    .body(Body::from(r#"{"mode":"full"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn branch_mode_diff_alone_validates() {
        let app = router(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/sandboxes/nope/branch")
                    .header("Content-Type", "application/json")
                    .body(Body::from(r#"{"mode":"diff"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn branch_mode_live_validates() {
        let app = router(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/sandboxes/nope/branch")
                    .header("Content-Type", "application/json")
                    .body(Body::from(r#"{"mode":"live"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn branch_mode_live_with_wait_false_validates() {
        let app = router(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/sandboxes/nope/branch")
                    .header("Content-Type", "application/json")
                    .body(Body::from(r#"{"mode":"live","wait":false}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        // Not 400 — `wait:false` is valid when `mode:live`. Falls
        // through to 404 since the sandbox doesn't exist.
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn branch_rejects_mode_full_with_wait_false() {
        // wait:false only makes sense for the live path.
        let app = router(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/sandboxes/anything/branch")
                    .header("Content-Type", "application/json")
                    .body(Body::from(r#"{"mode":"full","wait":false}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn branch_rejects_wait_false_without_live() {
        // Phase 6.4: wait=false only makes sense with live BRANCH (the
        // Full/Diff paths have nothing to background — their copy work
        // already happens inside the spawn_blocking task).
        for body in [
            r#"{"wait":false}"#,
            r#"{"wait":false,"diff":true}"#,
            r#"{"wait":false,"measure_diff":true}"#,
        ] {
            let app = router(test_state());
            let resp = app
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/v1/sandboxes/anything/branch")
                        .header("Content-Type", "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::BAD_REQUEST,
                "expected 400 for body: {body}",
            );
        }
    }

    #[tokio::test]
    async fn branch_rejects_multiple_modes_at_once() {
        // Phase 6.3: live + diff + measure_diff are mutually exclusive.
        for body in [
            r#"{"diff":true,"live":true}"#,
            r#"{"measure_diff":true,"live":true}"#,
            r#"{"diff":true,"measure_diff":true}"#,
            r#"{"diff":true,"measure_diff":true,"live":true}"#,
        ] {
            let app = router(test_state());
            let resp = app
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/v1/sandboxes/anything/branch")
                        .header("Content-Type", "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::BAD_REQUEST,
                "expected 400 for body: {body}",
            );
        }
    }

    // ------------------------------------------------------------
    // Review #282 round 3 — cancellation-safety regression tests
    // ------------------------------------------------------------

    /// Verify the core invariant the VmNetnsGuard relies on: after
    /// committing a reservation (VM enters live_vms) and then releasing
    /// the index (guard drops on cancellation or DELETE-during-BRANCH),
    /// the index is immediately reusable by a new reservation.
    #[test]
    fn committed_index_released_then_reusable() {
        let a = crate::netns::NetnsAllocator::new(4, Box::new(crate::netns::RangeProbe { max: 4 }));
        // Simulate create_sandbox: reserve → commit (VM in live_vms)
        let mut r1 = a.reserve(1).expect("reserve");
        let idx = r1.offset();
        r1.commit();
        // Simulate VmNetnsGuard::drop (cancellation or DELETE during BRANCH):
        // release_index makes the index available again.
        a.release_index(idx + 1); // reserve(1) covers index offset+1
                                  // The released index must be reusable.
        let r2 = a.reserve(1).expect("released index should be reusable");
        assert_eq!(r2.offset(), idx, "released index should be picked first");
    }

    /// Verify that an uncommitted reservation held inside a blocking
    /// task keeps indices reserved until the task completes, even if
    /// the "handler" drops its reference. This tests the create_sandbox
    /// fix: reserve() moved inside spawn_blocking so cancellation of the
    /// async handler doesn't release indices while the restore runs.
    #[test]
    fn uncommitted_reservation_in_blocking_task_blocks_concurrent_reserve() {
        let a = crate::netns::NetnsAllocator::new(2, Box::new(crate::netns::RangeProbe { max: 2 }));
        // Simulate: spawn_blocking task holds an uncommitted reservation
        let r1 = a.reserve(2).expect("reserve 2 in blocking task");
        // Concurrent request tries to reserve — must fail because the
        // blocking task still holds the range (even though it's only
        // reserved, not committed).
        assert!(
            a.reserve(1).is_none(),
            "concurrent reserve must not grab indices held by an in-flight blocking task"
        );
        // When the blocking task completes and drops its reservation
        // (failure path), the indices become available again.
        drop(r1);
        assert!(
            a.reserve(2).is_some(),
            "indices freed after blocking task drops reservation"
        );
    }

    /// Verify the full BRANCH/suspend lifecycle: reserve → commit →
    /// release (guard drop). The index cycles through all three
    /// allocator states (reserved → active → free) and returns to
    /// the available pool.
    #[test]
    fn netns_index_lifecycle_reserved_active_free() {
        let a = crate::netns::NetnsAllocator::new(1, Box::new(crate::netns::RangeProbe { max: 1 }));
        // Phase 1: reserve (blocking task starts)
        let mut r = a.reserve(1).expect("pool of 1 should satisfy");
        assert!(a.reserve(1).is_none(), "pool exhausted while reserved");
        // Phase 2: commit (VM inserted into live_vms)
        r.commit();
        assert!(a.reserve(1).is_none(), "pool exhausted while active");
        // Phase 3: release (VmNetnsGuard drops — BRANCH cancellation
        // or DELETE during take-out window)
        a.release_index(1);
        // Index is back in the available pool
        assert!(a.reserve(1).is_some(), "index reusable after release");
    }

    /// Regression for the `mem::forget` leak in `VmNetnsGuard::into_vm()`.
    /// The old implementation called `std::mem::forget(self)` after taking
    /// the VM, permanently leaking the guard's `Arc<NetnsAllocator>` on
    /// every successful BRANCH reinsertion. The fix lets the guard drop
    /// normally after `into_vm()` takes the VM out (Drop sees `vm == None`
    /// and skips kill+release). This test verifies the Arc strong count
    /// returns to baseline after the consumed guard is dropped — if the
    /// guard leaked the Arc, the count would stay elevated.
    #[test]
    fn vmnetnsguard_into_vm_does_not_leak_arc() {
        let alloc =
            crate::netns::NetnsAllocator::new(8, Box::new(crate::netns::RangeProbe { max: 8 }));
        // Baseline: only the test's own reference.
        let baseline = std::sync::Arc::strong_count(&alloc);

        // Simulate repeated BRANCH reinsertions: each iteration creates
        // a consumed guard (the post-`into_vm()` state) and drops it.
        // If each drop leaked the Arc, the strong count would grow
        // by N after N iterations. With the fix, it returns to baseline.
        for _ in 0..50 {
            let consumed = VmNetnsGuard::test_consumed(std::sync::Arc::clone(&alloc));
            drop(consumed);
        }
        assert_eq!(
            std::sync::Arc::strong_count(&alloc),
            baseline,
            "Arc strong count elevated after 50 consumed-guard drops — \
             into_vm() is leaking the allocator Arc"
        );
    }
}
