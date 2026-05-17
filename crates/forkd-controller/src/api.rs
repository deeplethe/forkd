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
