//! What the master and a node say to each other.
//!
//! The node dials out and asks for work; the master answers on that
//! connection. There is no master -> node connect anywhere in this protocol,
//! which is why a node needs no inbound port.

use serde::{Deserialize, Serialize};

use crate::tier::Tier;

/// What a node's fingerprint is made of, so an operator can see why two
/// nodes landed in different pools instead of comparing two hashes. Read in
/// the same pass as the fingerprint (`node::runtime::host`), which hashes the
/// engine, kernel, vendor and flags; `cpu_model` is shown but not hashed,
/// because the same feature set under another model name restores fine.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostInfo {
    /// `Backend::version`, e.g. `runsc version release-20261019.0`.
    pub engine: String,
    /// `/proc/sys/kernel/osrelease`.
    pub kernel: String,
    pub cpu_vendor: String,
    pub cpu_model: String,
    /// The `flags` line of `/proc/cpuinfo`, space separated.
    pub cpu_flags: String,
}

/// Node -> master, once, to trade a registration token for a certificate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterRequest {
    pub token: String,
    pub node_id: String,
    /// What this node is compatible with. Snapshots are only ever restored
    /// on a node with the same value; measured on bj/hk, a mismatch fails
    /// with `incompatible FeatureSet` and a match is still not a guarantee.
    pub fingerprint: String,
    pub max_sandboxes: u32,
    /// What `fingerprint` was computed from. Absent from older nodes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<HostInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterResponse {
    pub node_id: String,
    pub pool: String,
    /// PEM. The node writes these and uses them for every later call.
    pub client_cert: String,
    pub client_key: String,
    pub ca_cert: String,
    /// Bearer token for the node's later calls. Empty when the master is
    /// running unauthenticated.
    #[serde(default)]
    pub api_token: String,
}

// ---- listen mode: the master connects to the node ----------------------
//
// The same messages as the poll, carried the other way. A node that listens
// reports a `PollRequest` as its answer and is handed commands in the
// request, so the master's reconciliation runs unchanged whichever side
// opened the connection.

/// What a listening node says about itself when a master adds it. The same
/// facts registration carries in pull mode, minus the token: the pairing
/// code has already proved who is asking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentHello {
    pub node_id: String,
    /// Host fingerprint, which decides the pool exactly as it does when a
    /// node registers.
    pub fingerprint: String,
    pub max_sandboxes: u32,
    /// What `fingerprint` was computed from. Absent from older nodes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<HostInfo>,
}

/// What a master sends to a node it drives.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SyncRequest {
    /// `None` asks for a report and changes nothing. `Some` is a full batch:
    /// the node applies it and then forgets failures for any sandbox the
    /// batch did not mention, because the master has stopped asking about
    /// those. The distinction matters -- an empty list would wipe the
    /// failure book on every report-only refresh, and no sandbox would ever
    /// reach quarantine.
    #[serde(default)]
    pub commands: Option<Vec<Command>>,
}

/// What a listening node answers with.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncResponse {
    /// State AFTER applying the batch, so the master's next diff is computed
    /// against what is actually there now.
    pub report: PollRequest,
    /// Answers to exec and branch commands in the batch.
    #[serde(default)]
    pub results: Vec<CommandResult>,
    /// When the node next wants to retry something that is backing off, so
    /// the master's driver can come back then rather than on its refresh tick.
    #[serde(default)]
    pub retry_in_ms: Option<u64>,
}

/// Node -> master: "here is what I actually hold, give me work."
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PollRequest {
    pub node_id: String,
    /// Observed, not remembered. The master compares this against desired
    /// state rather than trusting its own cache.
    pub observed: Vec<ObservedSandbox>,
    pub free_memory_mib: u64,
    pub pressure_some_avg10: f64,
    /// How long the master may park this request when there is no work.
    /// The node sets it, shortening it when it has a retry due, so it can
    /// stay parked rather than sleeping. 0 means answer at once, which is
    /// what tests and one-shot probes want.
    #[serde(default)]
    pub hold_ms: Option<u64>,
    /// Commands that failed since the last call. Reported so the reason a
    /// sandbox is not coming up is visible in the API rather than only in
    /// one node's log.
    #[serde(default)]
    pub failures: Vec<CommandFailure>,
    /// What the node is putting off, and for how long: a retry that is not
    /// due, a demotion inside its grace, a fork waiting for its image. The
    /// master would only hand these straight back. On bare metal one failing
    /// sandbox had a node polling about 350 times a second, 40% of a core,
    /// until its retry came up.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deferred: Vec<Deferred>,
    /// Sandboxes the gateway forwarded a request to since the last report.
    ///
    /// The gateway wakes a cold sandbox itself when a request arrives for
    /// it (RFC §15.5: a wake never goes through the master), so this is how
    /// the master learns the sandbox is in use: it moves the idle clock and,
    /// for one that was woken, makes Hot the desired tier so the next diff
    /// does not put it straight back to sleep.
    #[serde(default)]
    pub traffic: Vec<String>,
    /// Sandboxes this node put to disk on its own, to make room for a wake.
    ///
    /// The master takes the node's word for these and moves desired state
    /// to the tier observed. If it asked for them back instead, the node
    /// would make room for that by putting away something else, and the
    /// two would take turns for ever.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evicted: Vec<String>,
    /// The density levers this node has on (`snapshot-compression`,
    /// `reclaim-hot`, `swap:zswap`, ...). Each node decides for itself from
    /// what its machine can do, so nodes in one pool can differ, and this is
    /// where that shows: `sandstorm_node_feature` on the master's /metrics.
    #[serde(default)]
    pub features: Vec<String>,
    /// Snapshot tags whose images are on this node's disk, baked here or
    /// fetched from another node. The master drops the ones it no longer
    /// has a record of, which is the only way an image ever leaves a node.
    /// A tag only appears here once its image is some minutes old, so one
    /// that a branch has just written -- before the master has recorded it
    /// -- is never mistaken for one that was deleted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// What this node's fingerprint is made of. Sent with every full report
    /// rather than only at registration, so nodes registered before it
    /// existed fill it in too; the master writes it only when it changes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<HostInfo>,
    /// This node's fingerprint now. Registration records it once, but an
    /// upgrade of runsc or the kernel changes it under a node that is already
    /// registered, and the master has to move that node out of a pool whose
    /// snapshots it can no longer restore (ADR 6). Absent from older nodes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    /// Templates this node can start on its own backend: those with a rootfs
    /// in the form that backend reads (`templates/<name>/rootfs` for runsc,
    /// `templates/<name>/rootfs.ext4` for Firecracker). The master places a
    /// template's sandboxes only on nodes that list it, so a create a group
    /// cannot run is refused rather than failed on a node. Absent from older
    /// nodes, which are placed on as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub templates: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Deferred {
    pub id: String,
    pub retry_in_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandFailure {
    pub id: String,
    pub error: String,
    /// How many times in a row this sandbox has failed. The node backs off
    /// on its own; this tells the master how bad it is.
    pub attempts: u32,
    /// Not a failure: the node held the command back for want of memory,
    /// and will start it once there is room. The master shows the reason
    /// but does not count it towards quarantine -- on 2026-10-27, 1,246 of
    /// 20,000 creates at 20/s were quarantined this way and never started,
    /// on nodes that had room for them minutes later.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub held_back: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObservedSandbox {
    pub id: String,
    pub tier: Tier,
    pub serving: bool,
    /// The address the node gave it. The node owns addressing, so this is
    /// what the master records and serves.
    #[serde(default)]
    pub ip: Option<String>,
    /// Which slot it holds, read back from the node's own records so an
    /// orphan can have its address reclaimed.
    #[serde(default)]
    pub net_index: Option<u32>,
}

/// Master -> node, in reply to a poll. Never a push.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PollResponse {
    pub commands: Vec<Command>,
}

/// A command the node should run once, as opposed to a tier the master
/// wants it to reach.
///
/// Tier changes are declarative: the master says where a sandbox should be
/// and the node converges on it, so a lost command is simply re-derived on
/// the next poll. `Exec` is not like that -- it runs once and a caller is
/// holding an HTTP request open waiting for its output. So it is queued and
/// delivered, not computed, and if it is lost the caller sees a timeout
/// rather than the command running twice.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Command {
    /// Bring a sandbox up from nothing.
    ///
    /// The spec travels with the command so a template is defined once, on
    /// the master. The rootfs does not: `template` only names a directory
    /// the node already has, so a master cannot point a node at an
    /// arbitrary path on its own disk.
    Create {
        id: String,
        template: String,
        spec: serde_json::Value,
        /// Port inside the sandbox. Not a host port any more: each sandbox
        /// has its own network namespace, so two may both use 8080.
        port: u16,
        volume: Option<String>,
        /// Slot in the node's address plan, which fixes the sandbox's IP.
        net_index: Option<u32>,
        /// Fork from this tag's checkpoint instead of starting cold. One
        /// image serves any number of children, so this is a restore, not
        /// a copy: measured at 45 ms each, five at once from one 14 MiB
        /// image.
        from_tag: Option<String>,
        /// Host port the node's gateway listens on for this sandbox.
        ///
        /// Unlike `port` this one IS a host port, so it is allocated per
        /// node and unique there. `None` leaves the sandbox reachable only
        /// from its own node, which is what every sandbox was before the
        /// gateway existed.
        #[serde(default)]
        gateway_port: Option<u16>,
        /// The sandbox's own access token, so the node's gateway can check
        /// it without asking the master on the data path -- RFC 15.5 is
        /// explicit that a request must not travel through the master.
        /// Carried over the mTLS control channel and stored at 0600.
        #[serde(default)]
        token: String,
        /// Where to fetch `from_tag`'s image if this node does not have it.
        /// Absent when the image is here, or for a cold start.
        /// Boxed: rarely present, and it would otherwise make every command
        /// as large as the largest.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fetch: Option<Box<FetchFrom>>,
    },
    /// Move an existing sandbox to a tier. One step at a time when demoting;
    /// promotion goes straight to hot.
    SetTier { id: String, tier: Tier },
    /// Remove it and everything it owns on this node, its network included.
    Destroy { id: String, net_index: Option<u32> },
    /// Checkpoint a running sandbox into a named tag, then put it back.
    ///
    /// runsc's checkpoint stops the container, so the parent is restored
    /// from the image it just wrote. That is a pause, not a live fork --
    /// the RFC lists a live branch's pause window as a non-goal, and this
    /// is the honest version of that trade.
    Branch {
        id: String,
        tag: String,
        /// Correlates the result with the caller waiting on it, exactly as
        /// for Exec.
        result_id: String,
    },
    /// Remove a tag's image from this node. Issued for a tag the node
    /// reports holding that the master no longer has.
    DropTag { tag: String },
    /// Run a command inside a sandbox and report back. `exec_id` is how the
    /// master matches the result to the caller still waiting on it.
    Exec {
        exec_id: String,
        id: String,
        args: Vec<String>,
        timeout_secs: u64,
    },
    /// Restart this node on another backend (`runsc` or `firecracker`).
    ///
    /// Only sent to a node that says it can (`backend-switch` in its
    /// features: it was installed with `--backend-file`), and only while the
    /// master holds no sandbox or snapshot on it -- every one of those is in
    /// the old backend's format and could never be restored. The node checks
    /// again for itself, runs the new backend's preflight, rewrites the file
    /// and exits for systemd to start it again; its fingerprint then names
    /// the new engine and it moves to that engine's pool. Answered like
    /// Branch, before the restart.
    SetBackend { backend: String, result_id: String },
}

/// Where a node fetches a tag's image from: the node that baked it.
///
/// That node's agent port, pinned to its certificate the same way the
/// master pins it, and a token good for this tag, this fetching node and
/// until `expires` (see `tag::fetch_token`). Not the pairing code, which
/// would let one node drive another.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchFrom {
    pub node_id: String,
    pub address: String,
    pub cert_sha256: String,
    /// The fetching node, which the token is bound to.
    pub for_node: String,
    pub expires: i64,
    pub token: String,
}

/// Node -> master, out of band: the outcome of one command a caller is
/// waiting on. Both `Exec` and `Branch` come back this way.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandResult {
    /// The `exec_id` or `result_id` from the command.
    #[serde(alias = "exec_id")]
    pub result_id: String,
    #[serde(default)]
    pub stdout: String,
    #[serde(default)]
    pub stderr: String,
    pub exit_code: i64,
    /// Bytes the branch's image occupies, for a Branch.
    #[serde(default)]
    pub size_bytes: u64,
}
