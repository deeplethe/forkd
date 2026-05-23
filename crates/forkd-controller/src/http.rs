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
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use parking_lot::Mutex;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::api::{
    BranchSandboxRequest, CreateSandboxRequest, CreateSnapshotRequest, CreateWorkspaceRequest,
    ErrorBody, EvalRequest, EvalResponse, ExecRequest, ExecResponse, SandboxInfo, SnapshotInfo,
    SuspendWorkspaceRequest, VersionResponse, WorkspaceInfo, WorkspaceStatus,
};
use crate::state::Registry;

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
    /// Scratch directory used for prewarm throwaway snapshots when
    /// `CreateSandboxRequest::prewarm` is set. Mirror of
    /// `DaemonConfig::prewarm_scratch_dir`.
    pub prewarm_scratch_dir: PathBuf,
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
    // Prometheus text format. Keep names stable — exporters depend on them.
    let body = format!(
        "# HELP forkd_snapshots_total Number of snapshots known to the controller.\n\
         # TYPE forkd_snapshots_total gauge\n\
         forkd_snapshots_total {snap_count}\n\
         # HELP forkd_sandboxes_active Number of active sandboxes (child VMs).\n\
         # TYPE forkd_sandboxes_active gauge\n\
         forkd_sandboxes_active {sb_count}\n\
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
    Json(s.registry.list_snapshots())
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
    };
    if let Err(e) = s.registry.insert_snapshot(info.clone()) {
        return server_error(&format!("persist snapshot: {e:#}"));
    }
    (StatusCode::CREATED, Json(info)).into_response()
}

async fn delete_snapshot(State(s): State<SharedState>, Path(tag): Path<String>) -> Response {
    // Sanity-guard the tag before touching disk paths.
    if !is_safe_tag(&tag) {
        return bad_request("tag must be 1-64 chars, ASCII alnum or dash/underscore");
    }
    let removed = match s.registry.remove_snapshot(&tag) {
        Ok(v) => v,
        Err(e) => return server_error(&format!("registry remove: {e}")),
    };
    // Even if it wasn't registered (e.g. created via CLI), still attempt
    // a disk cleanup so the daemon's DELETE is a single source of truth.
    let dir = s.snapshot_root.join(&tag);
    if dir.exists() {
        if let Err(e) = std::fs::remove_dir_all(&dir) {
            return server_error(&format!("rm {}: {e}", dir.display()));
        }
    } else if removed.is_none() {
        return not_found(&format!("snapshot {tag}"));
    }
    StatusCode::NO_CONTENT.into_response()
}

fn is_safe_tag(tag: &str) -> bool {
    !tag.is_empty()
        && tag.len() <= 64
        && tag
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
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
        Some(s) => PathBuf::from(&s.dir),
        None => {
            let dir = s.snapshot_root.join(&req.snapshot_tag);
            if !dir.join("vmstate").exists() {
                return not_found(&format!("snapshot {}", req.snapshot_tag));
            }
            dir
        }
    };
    // Prefer the persisted snapshot.json (carries volumes); fall back
    // to constructing from vmstate + memory.bin for backward compat
    // with snapshots written before the meta file existed.
    let snapshot = match std::fs::read(snap_dir.join("snapshot.json"))
        .ok()
        .and_then(|raw| serde_json::from_slice::<forkd_vmm::Snapshot>(&raw).ok())
    {
        Some(s) => s,
        None => forkd_vmm::Snapshot {
            vmstate: snap_dir.join("vmstate"),
            memory: snap_dir.join("memory.bin"),
            volumes: Vec::new(),
        },
    };

    let tag = req.snapshot_tag.clone();
    // Compute netns offset so we don't collide with other live sandboxes'
    // forkd-child-N indices. When per_child_netns is false this is a no-op.
    let netns_offset = if req.per_child_netns {
        pick_netns_offset(&s.live_vms.lock(), req.n)
    } else {
        0
    };
    let opts = forkd_vmm::ForkOpts {
        n: req.n,
        per_child_netns: req.per_child_netns,
        memory_limit_mib: req.memory_limit_mib,
        netns_offset,
        prewarm_scratch_dir: if req.prewarm {
            Some(s.prewarm_scratch_dir.clone())
        } else {
            None
        },
        // v0.2 ships only File. Userfault (live branching) lands in v0.3
        // — see docs/design/userfaultfd.md.
        memory_backend: forkd_vmm::MemoryBackend::File,
        // Daemon-spawned sources are the targets of BRANCH; enabling
        // dirty-page tracking lets later BRANCHes opt into Diff
        // snapshots (see docs/design/diff-snapshots.md). The cost is
        // ~1 bit per page; negligible.
        enable_diff_snapshots: true,
    };
    // Per-snapshot-tag work_dir would clash if two batches of the same tag
    // ran concurrently (e.g. two branches of the same source). Mix the
    // netns offset in so concurrent batches get distinct work_dirs.
    let work_dir = std::env::temp_dir().join(format!("forkd-daemon-{tag}-o{netns_offset}"));

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
    let prewarm_requested = req.prewarm;
    let fork_result = match tokio::task::spawn_blocking(move || {
        let mut last_err: Option<anyhow::Error> = None;
        let backoffs_ms = [50u64, 200, 800];
        for attempt in 0..=backoffs_ms.len() {
            if attempt > 0 {
                std::thread::sleep(std::time::Duration::from_millis(backoffs_ms[attempt - 1]));
            }
            match snapshot.restore_many_with(opts.clone(), &work_dir) {
                Ok(r) => return Ok(r),
                Err(e) => {
                    let msg = format!("{e:#}");
                    let is_busy = msg.contains("Resource busy")
                        || msg.contains("Device or resource busy")
                        || msg.contains("os error 16");
                    if !is_busy || attempt == backoffs_ms.len() {
                        return Err(e);
                    }
                    tracing::warn!(
                        attempt = attempt + 1,
                        next_backoff_ms = backoffs_ms[attempt],
                        error = %e,
                        "restore_many: tap/cgroup busy, retrying"
                    );
                    last_err = Some(e);
                }
            }
        }
        Err(last_err.expect("loop must produce an error on exit"))
    })
    .await
    {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => return server_error(&format!("restore_many: {e:#}")),
        Err(e) => return server_error(&format!("blocking task panicked: {e}")),
    };
    if prewarm_requested {
        tracing::info!(
            tag = %tag,
            n = fork_result.children.len(),
            spawn_ms = fork_result.spawn_ms as u64,
            restore_ms = fork_result.restore_ms as u64,
            prewarm_ms = fork_result.prewarm_ms as u64,
            "sandbox created (prewarmed)"
        );
    }

    let now = unix_now();
    let mut infos = Vec::with_capacity(fork_result.children.len());
    {
        let mut live = s.live_vms.lock();
        for vm in fork_result.children {
            let id = new_sandbox_id();
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
    }

    (StatusCode::CREATED, Json(infos)).into_response()
}

async fn delete_sandbox(State(s): State<SharedState>, Path(id): Path<String>) -> Response {
    // Drop kills the firecracker process and removes the cgroup leaf.
    let vm = s.live_vms.lock().remove(&id);
    let registered = match s.registry.remove_sandbox(&id) {
        Ok(v) => v,
        Err(e) => return server_error(&format!("registry remove: {e}")),
    };
    drop(vm);
    if registered.is_none() {
        return not_found(&format!("sandbox {id}"));
    }
    StatusCode::NO_CONTENT.into_response()
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
        .unwrap_or_else(|| format!("branch-{}-{}", id, unix_now()));
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

    if req.measure_diff && req.diff {
        return bad_request(
            "set at most one of `measure_diff` / `diff`: \
             measure_diff is the pure measurement hook (Full path + Diff sidecar timing); \
             diff is the real diff-based BRANCH path",
        );
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
    let vm = {
        let mut g = s.live_vms.lock();
        g.remove(&id)
    };
    let vm = match vm {
        Some(v) => v,
        None => return not_found(&format!("sandbox {id}")),
    };

    let snap_dir_for_task = snap_dir.clone();
    let id_for_log = id.clone();
    let measure_diff = req.measure_diff;
    let diff_mode = req.diff;
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
    let task_result = tokio::task::spawn_blocking(
        move || -> (
            forkd_vmm::Vm,
            anyhow::Result<forkd_vmm::Snapshot>,
            Option<u64>,
            DiffMetrics,
        ) {
            let mut pause_ms: Option<u64> = None;
            let mut diff_metrics: DiffMetrics = None;
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
            (vm, snap_result, pause_ms, diff_metrics)
        },
    )
    .await;

    let (vm_back, snap_or_err, pause_ms, diff_metrics) = match task_result {
        Ok(t) => t,
        Err(e) => {
            // Blocking task panicked; we lost the Vm value. The OS still has the
            // firecracker process running, but we no longer track it. Stale entry
            // will be reaped by Registry::reconcile on next pid-alive scan.
            return server_error(&format!("blocking task panicked: {e}"));
        }
    };

    // Re-insert the source sandbox into live_vms. If a DELETE happened during
    // the branching window, the entry is gone from the registry; we drop the
    // returned `vm` (its Drop kills firecracker + cleans cgroup).
    let mut new_branch_count: Option<u32> = None;
    if s.registry.get_sandbox(&id).is_some() {
        s.live_vms.lock().insert(id.clone(), vm_back);
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
        drop(vm_back);
    }

    let snap = match snap_or_err {
        Ok(s) => s,
        Err(e) => {
            // Best-effort cleanup of partial files.
            let _ = std::fs::remove_dir_all(&snap_dir);
            return server_error(&format!("branch: {e:#}"));
        }
    };

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
    // Advisory when the source's branch_count crosses into the slow
    // regime documented in issue #146. Threshold = 3 (BRANCH 1-2 are
    // typically fast; BRANCH 3+ shows the ~5× pause_ms jump).
    let warning = new_branch_count.filter(|&n| n >= 3).map(|n| {
        format!(
            "branch #{n} on this source — pause_ms typically grows 5× from BRANCH 3 (issue #146). \
             Consider spawning a fresh source from this BRANCH and continuing the chain there."
        )
    });
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
    };
    if let Err(e) = s.registry.insert_snapshot(info.clone()) {
        return server_error(&format!("persist snapshot: {e:#}"));
    }
    (StatusCode::CREATED, Json(info)).into_response()
}

/// Pick the smallest `netns_offset` such that
/// `[offset+1 .. offset+n+1]` is disjoint from every `forkd-child-K`
/// already registered in `live_vms`. Used to keep `POST /v1/sandboxes`
/// batches from clashing on netns indices (the original allocator
/// always started at 1, so a fork after a previous fork landed on
/// `forkd-child-1` again).
///
/// Off-by-one note: indices are 1-based on the wire (`forkd-child-1`,
/// not `forkd-child-0`); `netns_offset` is the *additive* offset
/// applied before the within-batch 1..=n loop in `restore_many_with`.
fn pick_netns_offset(live_vms: &HashMap<String, forkd_vmm::Vm>, n: usize) -> usize {
    let used: std::collections::HashSet<usize> = live_vms
        .values()
        .filter_map(|vm| vm.netns.as_ref())
        .filter_map(|s| s.strip_prefix("forkd-child-")?.parse::<usize>().ok())
        .collect();
    if used.is_empty() {
        return 0;
    }
    // Try offsets 0, 1, 2, … until [offset+1..offset+n+1] is disjoint.
    let mut offset = 0usize;
    loop {
        let range_start = offset + 1;
        let range_end = offset + n + 1;
        let clash = (range_start..range_end).any(|i| used.contains(&i));
        if !clash {
            return offset;
        }
        offset += 1;
    }
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

/// Short, URL-safe sandbox id. Not crypto-random; the daemon-only loopback
/// surface doesn't need unguessable ids. Switch to ULID if we ever expose
/// the API beyond localhost.
fn new_sandbox_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let ts = unix_now();
    format!("sb-{ts:x}-{n:04x}")
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
) -> anyhow::Result<(forkd_vmm::Vm, SandboxInfo)> {
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
        },
    };
    let netns_offset = if per_child_netns {
        pick_netns_offset(&s.live_vms.lock(), 1)
    } else {
        0
    };
    let opts = forkd_vmm::ForkOpts {
        n: 1,
        per_child_netns,
        memory_limit_mib,
        netns_offset,
        prewarm_scratch_dir: None,
        memory_backend: forkd_vmm::MemoryBackend::File,
        enable_diff_snapshots: true,
    };
    let work_dir =
        std::env::temp_dir().join(format!("forkd-workspace-{snapshot_tag}-o{netns_offset}"));
    let mut fork_result = snapshot.restore_many_with(opts, &work_dir)?;
    let vm = fork_result
        .children
        .pop()
        .ok_or_else(|| anyhow::anyhow!("restore_many returned no children"))?;

    let info = SandboxInfo {
        id: new_sandbox_id(),
        snapshot_tag: snapshot_tag.to_string(),
        netns: vm.netns.clone(),
        guest_addr: "10.42.0.2:8888".to_string(),
        created_at_unix: unix_now(),
        pid: Some(vm.pid()),
        memory_limit_mib,
        has_branched: false,
        last_branch_memory_path: None,
        branch_count: 0,
    };
    Ok((vm, info))
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

    let (vm, sb_info) = match spawn_result {
        Ok(Ok(pair)) => pair,
        Ok(Err(e)) => return server_error(&format!("spawn workspace sandbox: {e:#}")),
        Err(e) => return server_error(&format!("blocking task panicked: {e}")),
    };

    let id = sb_info.id.clone();
    if let Err(e) = s.registry.insert_sandbox(sb_info.clone()) {
        tracing::error!(error=%e, "persist workspace's live sandbox failed");
    }
    s.live_vms.lock().insert(id.clone(), vm);

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
            drop(vm); // Vm::drop kills firecracker + cleans cgroup
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
    let snap_dir_for_task = snap_dir.clone();
    let source_tag = ws.source_snapshot_tag.clone();
    let source_memory_path = s.snapshot_root.join(&source_tag).join("memory.bin");
    let last_chain = ws.last_branch_memory_path.clone();
    let diff_mode = req.diff;

    let task = tokio::task::spawn_blocking(move || -> (forkd_vmm::Vm, anyhow::Result<(forkd_vmm::Snapshot, Option<u64>)>) {
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

    let (vm_back, snap_or_err) = match task {
        Ok((vm, r)) => (vm, r),
        Err(e) => return server_error(&format!("blocking task panicked: {e}")),
    };

    // We took the VM out of live_vms for suspend; intentionally
    // discard it now (suspend == kill source after snapshotting).
    drop(vm_back);
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
    let spawn_result = tokio::task::spawn_blocking(move || {
        spawn_one_for_workspace(&s_clone, &spawn_tag, false, None)
    })
    .await;
    let (vm, sb_info) = match spawn_result {
        Ok(Ok(pair)) => pair,
        Ok(Err(e)) => return server_error(&format!("spawn workspace sandbox: {e:#}")),
        Err(e) => return server_error(&format!("blocking task panicked: {e}")),
    };
    let id = sb_info.id.clone();
    if let Err(e) = s.registry.insert_sandbox(sb_info.clone()) {
        tracing::error!(error=%e, "persist workspace's live sandbox failed");
    }
    s.live_vms.lock().insert(id.clone(), vm);

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

    fn test_state() -> SharedState {
        let td = tempfile::TempDir::new().unwrap();
        let path = td.path().join("state.json");
        let snapshot_root = td.path().join("snapshots");
        // Leak the TempDir so it survives the test (Drop deletes the dir).
        std::mem::forget(td);
        Arc::new(AppState {
            registry: Registry::load_or_init(path).unwrap(),
            live_vms: Mutex::new(HashMap::new()),
            snapshot_root,
            branch_in_flight: Mutex::new(HashSet::new()),
            branch_sem: Arc::new(Semaphore::new(DEFAULT_BRANCH_CONCURRENCY)),
            prewarm_scratch_dir: std::env::temp_dir().join("forkd-test-prewarm"),
        })
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
        let td = tempfile::TempDir::new().unwrap();
        let path = td.path().join("state.json");
        let snapshot_root = td.path().join("snapshots");
        std::mem::forget(td);
        let s = Arc::new(AppState {
            registry: Registry::load_or_init(path).unwrap(),
            live_vms: Mutex::new(HashMap::new()),
            snapshot_root,
            branch_in_flight: Mutex::new(HashSet::new()),
            branch_sem: Arc::new(Semaphore::new(2)),
            prewarm_scratch_dir: std::env::temp_dir().join("forkd-test-prewarm"),
        });
        let _a = s.try_acquire_branch_slot("t1").unwrap();
        let _b = s.try_acquire_branch_slot("t2").unwrap();
        let err = s
            .try_acquire_branch_slot("t3")
            .expect_err("third slot should be refused");
        assert_eq!(err, BranchSlotError::CapacityExceeded);
    }

    #[test]
    fn branch_slot_capacity_recovers_on_drop() {
        let td = tempfile::TempDir::new().unwrap();
        let path = td.path().join("state.json");
        let snapshot_root = td.path().join("snapshots");
        std::mem::forget(td);
        let s = Arc::new(AppState {
            registry: Registry::load_or_init(path).unwrap(),
            live_vms: Mutex::new(HashMap::new()),
            snapshot_root,
            branch_in_flight: Mutex::new(HashSet::new()),
            branch_sem: Arc::new(Semaphore::new(1)),
            prewarm_scratch_dir: std::env::temp_dir().join("forkd-test-prewarm"),
        });
        let a = s.try_acquire_branch_slot("t1").unwrap();
        assert!(s.try_acquire_branch_slot("t2").is_err());
        drop(a);
        let _b = s
            .try_acquire_branch_slot("t2")
            .expect("slot should free up after Drop");
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
}
