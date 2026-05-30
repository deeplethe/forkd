//! HTTP request/response types for the forkd controller REST API.
//!
//! API version: v1. Stable within v0.0.x patches. Breaking changes bump
//! the URL prefix (`/v2/...`).
use serde::{Deserialize, Serialize};

/// `POST /v1/snapshots` — build a snapshot from kernel + rootfs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSnapshotRequest {
    pub tag: String,
    pub kernel: String,
    pub rootfs: String,
    #[serde(default)]
    pub rw: bool,
    #[serde(default)]
    pub tap: Option<String>,
    #[serde(default = "default_boot_wait")]
    pub boot_wait_secs: u64,
}

fn default_boot_wait() -> u64 {
    10
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotInfo {
    pub tag: String,
    pub dir: String,
    pub created_at_unix: u64,
    /// Set when this snapshot was produced by branching from a running
    /// sandbox via `POST /v1/sandboxes/:id/branch`. Carries the source
    /// sandbox id for audit / lineage. None for snapshots built from
    /// kernel + rootfs via `POST /v1/snapshots`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branched_from: Option<String>,
    /// For BRANCH-produced snapshots: the source-VM pause window in
    /// milliseconds, measured around `pause() → resume()`. This is
    /// the daemon's ground-truth time the source was inactive — the
    /// application-observed pause (TCP stalls, missed pings) can
    /// be longer due to OS retransmit timers and shorter for
    /// short-pause workloads if the agent times its own retries.
    /// None for non-BRANCH snapshots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause_ms: Option<u64>,
    /// Phase 1a diff-snapshot measurement (when `measure_diff: true`
    /// was set on the BRANCH request): time spent in the Diff
    /// snapshot's `snapshot/create` call. Taken FIRST inside the
    /// pause window, so this is a strict subset of `pause_ms`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff_ms: Option<u64>,
    /// Phase 1a diff-snapshot measurement: on-disk allocated bytes of
    /// the Diff snapshot file (= dirty page bytes). Pair with
    /// `diff_logical_bytes` to compute the compression ratio.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff_physical_bytes: Option<u64>,
    /// Phase 1a diff-snapshot measurement: logical size of the Diff
    /// snapshot file. Equals the source's full guest-RAM size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff_logical_bytes: Option<u64>,
    /// Human-readable advisory included on BRANCH responses when the
    /// source sandbox has been BRANCHed 3+ times. Issue #146 documents
    /// a ~5× pause_ms jump in that regime. None for non-BRANCH
    /// snapshots and for the first 2 BRANCHes on any source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    /// Phase 6.4: state of the snapshot's on-disk content. Always
    /// `Ready` for snapshots produced by the synchronous Full / Diff
    /// paths or by live-BRANCH with `wait: true`. For live-BRANCH with
    /// `wait: false`, transitions `Writing -> Ready` once the background
    /// bulk copier finishes, or `Writing -> Failed` if the copier
    /// errors. The `Writing` state is **in-memory only** for v0.4;
    /// daemon restarts during a write-in-flight surface as the
    /// snapshot simply not appearing in the registry (the user must
    /// re-BRANCH).
    #[serde(default = "default_snapshot_status")]
    pub status: SnapshotStatus,
}

fn default_snapshot_status() -> SnapshotStatus {
    SnapshotStatus::Ready
}

/// Phase 6.4: per-snapshot lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SnapshotStatus {
    /// `memory.bin` is still being streamed by a `wait: false` live
    /// BRANCH. The vmstate header is on disk but the snapshot isn't
    /// restorable yet — `POST /v1/sandboxes` (fork) on this tag will
    /// 409.
    Writing,
    /// Snapshot is complete and ready to be restored.
    Ready,
    /// Background copier errored mid-write. The on-disk files may be
    /// partial; the registry entry is kept for diagnostic purposes
    /// but is not restorable.
    Failed,
}

/// `POST /v1/sandboxes/:id/branch` — pause a running sandbox, snapshot
/// it into a new tag, resume it. The resulting snapshot is independent
/// of the source sandbox's lifecycle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchSandboxRequest {
    /// Optional tag for the new snapshot. When unset the controller
    /// generates `branch-<source-id>-<unix-ts>`.
    #[serde(default)]
    pub tag: Option<String>,
    /// Phase 1a measurement hook: take a Diff snapshot in addition to
    /// the Full snapshot, and report timing + physical size for both
    /// in the response. The Diff is taken FIRST (so it captures the
    /// full dirty-since-restore bitmap), then the Full is taken (which
    /// would have been taken anyway). The Diff file is discarded
    /// immediately after measurement.
    ///
    /// Doesn't change snapshot semantics — the returned `SnapshotInfo`
    /// still references the Full snapshot. Used by
    /// `bench/pause-window/sweep-diff.sh` to A/B the two paths on the
    /// same source. Phase 1b will replace this with a real diff-based
    /// BRANCH path that produces a restorable shadow file.
    #[serde(default)]
    pub measure_diff: bool,
    /// Phase 1b: take a Diff snapshot during pause and reconstruct the
    /// full memory.bin asynchronously around it. The source's pause
    /// window shrinks to the Diff write (~250 ms for an idle source)
    /// while total BRANCH wall-clock stays roughly O(memory size) —
    /// the difference is that the source keeps running during the
    /// O(memory) copy work.
    ///
    /// Concrete sequence in the daemon:
    /// 1. Kick off a background `std::fs::copy(source_tag/memory.bin →
    ///    snap_dir/memory.bin)`. Source is still running during this.
    /// 2. `pause()` source.
    /// 3. `snapshot_diff_to(snap_dir/vmstate, /tmp/diff.bin)` — the
    ///    only thing the user actually waits on.
    /// 4. `resume()` source.
    /// 5. Wait for step 1 to finish.
    /// 6. `apply_diff(diff.bin, snap_dir/memory.bin)` — small write.
    ///
    /// Mutually exclusive with `measure_diff` (which is a pure
    /// measurement hook, doesn't change the snapshot path). When both
    /// are set, the daemon errors with 400.
    #[serde(default)]
    pub diff: bool,
    /// **Phase 6.3 unstable / internal.** Take a live BRANCH: arm
    /// UFFD_WP on the source's memfd-backed memory, dump only vmstate
    /// inside the pause window (`SnapshotType::VmstateOnly`), resume
    /// the source, and stream `memory.bin` asynchronously while the
    /// source keeps running. Target pause window is < 10 ms.
    ///
    /// Requires the sandbox to have been spawned with
    /// `MemoryBackend::MemfdShared` (Phase 5b) — file-backed
    /// sandboxes don't support UFFD_WP and the request fails 400.
    /// Mutually exclusive with `diff` and `measure_diff`.
    ///
    /// **The wire-level name and shape will change in Phase 7** when
    /// the public surface lands as `mode: "live" / "diff" / "full"`.
    /// Don't rely on `live: true` from external clients yet — it's
    /// here so the controller smoke test and the dev-box benchmark
    /// can exercise the path before Phase 7 rewrites it.
    #[serde(default)]
    pub live: bool,
    /// Phase 6.4: when `false`, the live-BRANCH response returns as
    /// soon as the source resumes (~10 ms), and the bulk copy from
    /// memfd into `memory.bin` continues in the background. The
    /// snapshot's `SnapshotInfo.status` reports `Writing` until the
    /// background copier finishes, then flips to `Ready`. Forking
    /// from the tag (`POST /v1/sandboxes`) while the snapshot is
    /// `Writing` returns 409.
    ///
    /// Only meaningful with `live: true`. Setting `wait: false`
    /// without `live: true` returns 400.
    ///
    /// Default: `true` (synchronous, current behavior).
    #[serde(default = "default_wait")]
    pub wait: bool,
}

fn default_wait() -> bool {
    true
}

/// `POST /v1/sandboxes` — fork a sandbox (child VM) from a snapshot tag.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSandboxRequest {
    pub snapshot_tag: String,
    #[serde(default = "default_one")]
    pub n: usize,
    #[serde(default)]
    pub per_child_netns: bool,
    /// Optional memory limit in MiB. Enforced via cgroup v2 if available.
    #[serde(default)]
    pub memory_limit_mib: Option<u64>,
    /// If true, immediately after each child is restored, perform a
    /// throwaway snapshot to scratch storage to fault-in all guest pages
    /// and populate KVM EPT. This amortizes the cold-cache penalty (2-9x
    /// slower first BRANCH vs. subsequent ones — see
    /// `bench/pause-window/RESULTS-v0.2.md`) so the first user-visible
    /// BRANCH on this sandbox runs at steady-state speed.
    ///
    /// The scratch directory is the daemon's `prewarm_scratch_dir`
    /// config setting (default `/dev/shm/forkd-prewarm`). If unavailable,
    /// the request fails — better to surface the config issue than to
    /// silently skip the prewarm.
    ///
    /// Trade-off: adds one tmpfs-grade pause-window (≈170 ms / 512 MiB,
    /// ≈1.3 s / 4 GiB) per child to sandbox creation in exchange for a
    /// consistent BRANCH latency from the first call.
    #[serde(default)]
    pub prewarm: bool,
}

fn default_one() -> usize {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxInfo {
    pub id: String,
    pub snapshot_tag: String,
    pub netns: Option<String>,
    pub guest_addr: String,
    pub created_at_unix: u64,
    pub pid: Option<u32>,
    pub memory_limit_mib: Option<u64>,
    /// Set to true once any BRANCH (Full or Diff) has been taken from
    /// this sandbox. Diagnostic flag — phase 1d (v0.3.1) lifted the
    /// "first BRANCH only" diff restriction by tracking
    /// `last_branch_memory_path` instead.
    #[serde(default)]
    pub has_branched: bool,
    /// Path to the memory.bin of this sandbox's most recent BRANCH
    /// output. When set and the file still exists, the daemon uses it
    /// as the base for the next `diff: true` BRANCH (instead of the
    /// source tag's memory.bin). This makes diff BRANCH correct for
    /// the Nth BRANCH on a sandbox, not just the first:
    ///
    ///   - BRANCH 1: cp source_tag/memory.bin → snap_dir_1/memory.bin;
    ///     diff captures dirty-since-restore; apply diff → BRANCH 1
    ///     output reflects state at BRANCH 1 pause.
    ///   - BRANCH 2: cp snap_dir_1/memory.bin → snap_dir_2/memory.bin;
    ///     diff captures dirty-since-BRANCH-1; apply diff → BRANCH 2
    ///     output reflects state at BRANCH 2 pause. ✓
    ///
    /// If the user deletes snap_dir_1 between BRANCHes, the daemon
    /// detects the missing file and falls back to source_tag (correct
    /// for boot-state recovery, semantically lossy — pages dirtied
    /// before the deletion are lost from the chain).
    ///
    /// Updated after every successful BRANCH (Full or Diff). Persisted
    /// via Registry::update_last_branch_memory_path.
    #[serde(default)]
    pub last_branch_memory_path: Option<std::path::PathBuf>,
    /// Total number of BRANCHes taken on this sandbox (Full + Diff).
    /// Incremented in `mark_branched`. Originally added to surface the
    /// multi-BRANCH pause anomaly tracked in
    /// [#146](https://github.com/deeplethe/forkd/issues/146) — that
    /// anomaly was fixed in v0.3.4 (the posix_fallocate path in
    /// `branch_sandbox`), so the counter is now purely informational.
    /// Kept in `SandboxInfo` because `forkd ls` displays it and some
    /// downstream operators may want it for cost / lineage tracking.
    #[serde(default)]
    pub branch_count: u32,
}

/// State of a stateful workspace (#116). Tracks whether the workspace
/// is currently driving a live sandbox or has been suspended to a
/// state tag (so a future `resume` can pick up where it left off).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkspaceStatus {
    /// Has a live sandbox (`live_sandbox_id` is Some).
    Running,
    /// No live sandbox; `current_state_tag` points at the latest
    /// suspended snapshot. `resume` spawns from there.
    Suspended,
    /// Was Running at daemon shutdown / crash. The live sandbox is
    /// gone; the workspace needs a fresh resume from
    /// `current_state_tag` (if any) or `source_snapshot_tag` (if
    /// never suspended).
    Stale,
}

/// `POST /v1/workspaces` — create a new stateful workspace.
///
/// Spawns a sandbox from `snapshot_tag` and tracks it as a workspace
/// the user can `suspend` / `resume` across daemon restarts. The
/// workspace is identified by `name` (unique per daemon).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateWorkspaceRequest {
    pub name: String,
    pub snapshot_tag: String,
    #[serde(default)]
    pub per_child_netns: bool,
    #[serde(default)]
    pub memory_limit_mib: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    pub id: String,
    pub name: String,
    pub source_snapshot_tag: String,
    /// Set after the first successful `suspend`. None for workspaces
    /// that have only been Running since creation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_state_tag: Option<String>,
    pub status: WorkspaceStatus,
    /// Set when status == Running. None otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_sandbox_id: Option<String>,
    pub created_at_unix: u64,
    pub last_active_unix: u64,
    /// Persisted between resumes — used to chain diff snapshots
    /// across the workspace lifetime if the operator opts in via
    /// `suspend?diff=true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_branch_memory_path: Option<std::path::PathBuf>,
}

/// `POST /v1/workspaces/:name/suspend` request body.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SuspendWorkspaceRequest {
    /// Use v0.3 diff snapshot for the suspend write. ~200 ms source
    /// pause vs seconds for a Full snapshot. Honors the same
    /// `last_branch_memory_path` chain that `POST /v1/sandboxes/:id/branch`
    /// uses.
    #[serde(default)]
    pub diff: bool,
}

/// `POST /v1/sandboxes/:id/exec`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecRequest {
    pub args: Vec<String>,
    #[serde(default = "default_exec_timeout")]
    pub timeout_secs: u64,
}

fn default_exec_timeout() -> u64 {
    30
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecResponse {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i64,
}

/// `POST /v1/sandboxes/:id/eval`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalRequest {
    pub code: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalResponse {
    pub result: Option<String>,
    pub error: Option<String>,
    pub exit_code: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionResponse {
    pub version: String,
    pub api: String,
}
