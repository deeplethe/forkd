//! What the master and a node say to each other.
//!
//! The node dials out and asks for work; the master answers on that
//! connection. There is no master -> node connect anywhere in this protocol,
//! which is why a node needs no inbound port.

use serde::{Deserialize, Serialize};

use crate::tier::Tier;

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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandFailure {
    pub id: String,
    pub error: String,
    /// How many times in a row this sandbox has failed. The node backs off
    /// on its own; this tells the master how bad it is.
    pub attempts: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObservedSandbox {
    pub id: String,
    pub tier: Tier,
    pub serving: bool,
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
        port: u16,
        volume: Option<String>,
    },
    /// Move an existing sandbox to a tier. One step at a time when demoting;
    /// promotion goes straight to hot.
    SetTier { id: String, tier: Tier },
    /// Remove it and everything it owns on this node.
    Destroy { id: String },
    /// Run a command inside a sandbox and report back. `exec_id` is how the
    /// master matches the result to the caller still waiting on it.
    Exec {
        exec_id: String,
        id: String,
        args: Vec<String>,
        timeout_secs: u64,
    },
}

/// Node -> master, out of band: the output of one `Exec`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecResult {
    pub exec_id: String,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i64,
}
