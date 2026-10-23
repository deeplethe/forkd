//! The sandbox agent.
//!
//! Registers once, then polls the master for work. It never listens for the
//! master, so a node machine needs no inbound port at all -- which is both
//! the simplest firewall story and the reason a compromised master cannot
//! reach a machine that has not called in.

mod runtime;

pub use runtime::{Runtime, RuntimeConfig};

use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::time::Duration;

use crate::proto::{
    Command, ObservedSandbox, PollRequest, PollResponse, RegisterRequest, RegisterResponse,
};

pub struct NodeConfig {
    pub node_id: String,
    pub master_url: String,
    /// Registration token, needed only until a client certificate exists.
    pub token: Option<String>,
    pub pki_dir: PathBuf,
    pub runtime: RuntimeConfig,
    pub poll_interval: Duration,
}

pub async fn serve(cfg: NodeConfig) -> Result<()> {
    std::fs::create_dir_all(&cfg.pki_dir)?;
    let cert_path = cfg.pki_dir.join("client.pem");
    let key_path = cfg.pki_dir.join("client-key.pem");

    if !cert_path.exists() || !key_path.exists() {
        let Some(token) = cfg.token.as_deref() else {
            bail!(
                "node has no client certificate and no registration token; \
                 get one from the master with `forkd node-token create`"
            );
        };
        register(&cfg, token, &cert_path, &key_path).await?;
    } else {
        tracing::info!(node = %cfg.node_id, "using existing client certificate");
    }

    let rt = Runtime::new(cfg.runtime.clone())?;
    // Adoption, not reconciliation: whatever is already running here is
    // reported as observed and kept. Measured on bm, a kill -9 of the
    // control process cost 0 of 20 sandboxes this way.
    let adopted = rt.observe()?;
    if !adopted.is_empty() {
        tracing::info!(count = adopted.len(), "re-adopted running sandboxes");
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;
    let mut tick = tokio::time::interval(cfg.poll_interval);
    loop {
        tick.tick().await;
        if let Err(e) = poll_once(&cfg, &client, &rt).await {
            tracing::warn!(error = %e, "poll failed");
        }
    }
}

async fn register(
    cfg: &NodeConfig,
    token: &str,
    cert_path: &PathBuf,
    key_path: &PathBuf,
) -> Result<()> {
    let req = RegisterRequest {
        token: token.to_string(),
        node_id: cfg.node_id.clone(),
        fingerprint: runtime::fingerprint()?,
        max_sandboxes: cfg.runtime.max_sandboxes,
    };
    let url = format!("{}/v1/node/register", cfg.master_url.trim_end_matches('/'));
    let resp = reqwest::Client::new()
        .post(&url)
        .json(&req)
        .send()
        .await
        .with_context(|| format!("registering with {url}"))?;
    if !resp.status().is_success() {
        bail!(
            "registration refused: {} {}",
            resp.status(),
            resp.text().await?
        );
    }
    let r: RegisterResponse = resp.json().await?;
    std::fs::write(cert_path, &r.client_cert)?;
    write_private(key_path, &r.client_key)?;
    std::fs::write(cfg.pki_dir.join("ca.pem"), &r.ca_cert)?;
    tracing::info!(node = %r.node_id, pool = %r.pool, "registered");
    Ok(())
}

fn write_private(path: &PathBuf, pem: &str) -> Result<()> {
    std::fs::write(path, pem)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

async fn poll_once(cfg: &NodeConfig, client: &reqwest::Client, rt: &Runtime) -> Result<()> {
    let observed: Vec<ObservedSandbox> = rt.observe()?;
    let (free_mib, psi) = runtime::host_pressure();
    let req = PollRequest {
        node_id: cfg.node_id.clone(),
        observed,
        free_memory_mib: free_mib,
        pressure_some_avg10: psi,
    };
    let url = format!("{}/v1/node/poll", cfg.master_url.trim_end_matches('/'));
    let resp: PollResponse = client.post(&url).json(&req).send().await?.json().await?;

    for cmd in resp.commands {
        let out = match &cmd {
            Command::Create {
                id,
                template,
                port,
                volume,
            } => rt.create(id, template, *port, volume.as_deref()),
            Command::SetTier { id, tier } => rt.set_tier(id, *tier),
            Command::Destroy { id } => rt.destroy(id),
        };
        if let Err(e) = out {
            // One bad command must not stop the others; the master will
            // re-issue it on the next poll from fresh observation.
            tracing::warn!(error = %e, ?cmd, "command failed");
        }
    }
    Ok(())
}
