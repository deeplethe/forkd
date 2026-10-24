//! The control plane.
//!
//! Holds desired state in sqlite, answers the public API, issues node
//! identities, and runs the demotion loop. It never connects to a node.

mod api;
mod demote;

pub use api::{router, Admission, MasterState, MAX_FAILURES};
pub use demote::demotion_loop;

use anyhow::Result;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use crate::identity::Ca;
use crate::state::Store;
use crate::tier::IdlePolicy;

pub struct MasterConfig {
    pub bind: SocketAddr,
    pub state_db: PathBuf,
    pub pki_dir: PathBuf,
    pub idle: IdlePolicy,
    pub port_range: (u16, u16),
    pub admit: Admission,
    /// Bearer token for the API. Generated on first run when absent.
    pub api_token: Option<String>,
}

/// Start the master. Returns once the listener is bound; the caller drives
/// the returned futures.
pub async fn serve(cfg: MasterConfig) -> Result<()> {
    let store = Store::open(&cfg.state_db)?;
    // Nothing has polled in yet, whatever the last run believed.
    let stale = store.mark_all_nodes_disconnected()?;
    if stale > 0 {
        tracing::info!(
            nodes = stale,
            "marked nodes disconnected pending their next poll"
        );
    }
    let ca = Ca::open_or_create(&cfg.pki_dir)?;
    // The master's own server cert: nodes verify this, so a fake master
    // cannot collect node connections.
    let _server = ca.issue_master(vec![
        "127.0.0.1".to_string(),
        "localhost".to_string(),
        cfg.bind.ip().to_string(),
    ])?;

    let st = Arc::new(MasterState::new(
        store,
        ca,
        cfg.idle,
        cfg.port_range,
        cfg.admit,
        cfg.api_token.clone(),
    ));

    let demoter = tokio::spawn(demotion_loop(st.clone()));
    let listener = tokio::net::TcpListener::bind(cfg.bind).await?;
    tracing::info!(bind = %cfg.bind, "master listening");
    let serve = axum::serve(listener, router(st));

    tokio::select! {
        r = serve => r?,
        r = demoter => r??,
    }
    Ok(())
}
