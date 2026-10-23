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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Command {
    /// Bring a sandbox up from nothing.
    Create {
        id: String,
        template: String,
        port: u16,
        volume: Option<String>,
    },
    /// Move an existing sandbox to a tier. One step at a time when demoting;
    /// promotion goes straight to hot.
    SetTier { id: String, tier: Tier },
    /// Remove it and everything it owns on this node.
    Destroy { id: String },
}
