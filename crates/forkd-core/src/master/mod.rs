//! The control plane.
//!
//! Holds desired state in sqlite, answers the public API, issues node
//! identities, and runs the demotion loop. It never connects to a node.

mod api;
mod demote;
pub mod metrics;

pub use api::{
    router, Admission, MasterOptions, MasterState, DEFAULT_SANDBOX_PORT, MAX_FAILURES,
    SANDBOX_TOKEN_HEADER,
};
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
    pub max_net_index: u32,
    /// Serve plain HTTP and accept nodes with no certificate.
    ///
    /// The control plane carries node registration tokens and every command
    /// run inside a sandbox, so this is for a single box on loopback and
    /// nothing else. `serve` logs what it gives up.
    pub insecure_transport: bool,
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
    let server_identity = crate::tls::load_or_issue_master(
        &ca,
        vec![
            "127.0.0.1".to_string(),
            "localhost".to_string(),
            cfg.bind.ip().to_string(),
        ],
    )?;
    let ca_fingerprint = crate::tls::fingerprint_of_pem(&server_identity.2)?;

    let st = Arc::new(MasterState::new(
        store,
        ca,
        MasterOptions {
            idle: cfg.idle,
            port_range: cfg.port_range,
            admit: cfg.admit,
            api_token: cfg.api_token.clone(),
            max_net_index: cfg.max_net_index,
            require_node_certs: !cfg.insecure_transport,
        },
    ));

    let demoter = tokio::spawn(demotion_loop(st.clone()));
    let listener = std::net::TcpListener::bind(cfg.bind)?;
    listener.set_nonblocking(true)?;

    if cfg.insecure_transport {
        tracing::warn!(
            bind = %cfg.bind,
            "--insecure-transport: the control plane is plain HTTP. Node \
             registration tokens, the API token and every command run inside \
             a sandbox cross the network in the clear, and any machine that \
             can reach this port can accept scheduling."
        );
        let listener = tokio::net::TcpListener::from_std(listener)?;
        let serve = axum::serve(listener, router(st));
        tokio::select! {
            r = serve => r?,
            r = demoter => r??,
        }
        return Ok(());
    }

    let (cert, key, ca_pem) = server_identity;
    crate::tls::install_crypto_provider();
    let acceptor =
        crate::tls::PeerCertAcceptor::new(crate::tls::server_config(&cert, &key, &ca_pem)?);
    tracing::info!(
        bind = %cfg.bind,
        ca = %ca_fingerprint,
        "master listening over TLS; nodes pin this CA fingerprint on first contact"
    );
    let serve = axum_server::from_tcp(listener)?
        .acceptor(acceptor)
        .serve(router(st).into_make_service());

    tokio::select! {
        r = serve => r?,
        r = demoter => r??,
    }
    Ok(())
}
