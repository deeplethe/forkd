//! The sandbox agent.
//!
//! Registers once, then polls the master for work. It never listens for the
//! master, so a node machine needs no inbound port at all -- which is both
//! the simplest firewall story and the reason a compromised master cannot
//! reach a machine that has not called in.

mod bundle;
mod runtime;

pub use bundle::TemplateSpec;
pub use runtime::{Runtime, RuntimeConfig};

use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::proto::{
    Command, CommandFailure, ExecResult, ObservedSandbox, PollRequest, PollResponse,
    RegisterRequest, RegisterResponse,
};

/// A sandbox whose command keeps failing.
///
/// Without this, a command that can never succeed spins: the node fails,
/// still does not hold the sandbox, the master re-issues the same command at
/// once, and round it goes. The old 5 s poll interval hid this by throttling
/// every retry to once per tick; holding the connection open took that
/// accidental limiter away, so the backoff has to be explicit.
struct Failing {
    attempts: u32,
    retry_at: Instant,
    error: String,
}

const BACKOFF_MIN: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(60);

fn backoff_for(attempts: u32) -> Duration {
    BACKOFF_MIN
        .saturating_mul(1u32 << attempts.min(7))
        .min(BACKOFF_MAX)
}

pub struct NodeConfig {
    pub node_id: String,
    pub master_url: String,
    /// Registration token, needed only until a client certificate exists.
    pub token: Option<String>,
    /// Bearer token for the master's guarded routes. Learned at
    /// registration when not supplied, and cached beside the certificate.
    pub api_token: Option<String>,
    pub pki_dir: PathBuf,
    pub runtime: RuntimeConfig,
    /// How long the master may park a request that has no work for us.
    /// This is a connection refresh interval, not a scheduling delay:
    /// anything the master wants arrives on the parked request at once.
    pub hold: Duration,
}

pub async fn serve(mut cfg: NodeConfig) -> Result<()> {
    std::fs::create_dir_all(&cfg.pki_dir)?;
    let cert_path = cfg.pki_dir.join("client.pem");
    let key_path = cfg.pki_dir.join("client-key.pem");
    let api_token_path = cfg.pki_dir.join("api-token");

    if !cert_path.exists() || !key_path.exists() {
        let Some(token) = cfg.token.as_deref() else {
            bail!(
                "node has no client certificate and no registration token; \
                 get one from the master with `forkd node-token create`"
            );
        };
        let learned = register(&cfg, token, &cert_path, &key_path).await?;
        if cfg.api_token.is_none() && !learned.is_empty() {
            write_private(&api_token_path, &learned)?;
            cfg.api_token = Some(learned);
        }
    } else {
        tracing::info!(node = %cfg.node_id, "using existing client certificate");
        if cfg.api_token.is_none() {
            if let Ok(t) = std::fs::read_to_string(&api_token_path) {
                let t = t.trim().to_string();
                if !t.is_empty() {
                    cfg.api_token = Some(t);
                }
            }
        }
    }

    let rt = Runtime::new(cfg.runtime.clone())?;
    // Adoption, not reconciliation: whatever is already running here is
    // reported as observed and kept. Measured on bm, a kill -9 of the
    // control process cost 0 of 20 sandboxes this way.
    let adopted = rt.observe()?;
    if !adopted.is_empty() {
        tracing::info!(count = adopted.len(), "re-adopted running sandboxes");
    }

    // The request is parked by the master, so the client timeout has to
    // outlast the hold or we would hang up on our own connection.
    let client = reqwest::Client::builder()
        .timeout(cfg.hold + Duration::from_secs(15))
        .build()?;

    tracing::info!(
        node = %cfg.node_id,
        hold_s = cfg.hold.as_secs(),
        "accepting scheduling"
    );

    // Reconnect immediately on success: the master is holding the next
    // request open, so this loop spends its life parked, not spinning.
    let mut net_backoff = Duration::from_millis(250);
    let mut failing: HashMap<String, Failing> = HashMap::new();
    // How long to let the master park the next request. Shortened when a
    // retry is due, so the node waits by STAYING PARKED rather than by
    // sleeping -- sleeping would make one sandbox's backoff delay dispatch
    // for every other sandbox on this node.
    let mut next_hold = cfg.hold;
    loop {
        match poll_once(&cfg, &client, &rt, &mut failing, next_hold).await {
            Ok(retry_due) => {
                net_backoff = Duration::from_millis(250);
                next_hold = retry_due.map_or(cfg.hold, |d| d.min(cfg.hold));
            }
            Err(e) => {
                tracing::warn!(error = %e, backoff_ms = net_backoff.as_millis(), "stream failed");
                tokio::time::sleep(net_backoff).await;
                net_backoff = (net_backoff * 2).min(Duration::from_secs(15));
                next_hold = cfg.hold;
            }
        }
    }
}

/// Returns the API token the master handed back, if any.
async fn register(
    cfg: &NodeConfig,
    token: &str,
    cert_path: &PathBuf,
    key_path: &PathBuf,
) -> Result<String> {
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
    Ok(r.api_token)
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

/// One trip to the master. Returns when the soonest backed-off retry comes
/// due, so the caller can shorten the next hold to match.
async fn poll_once(
    cfg: &NodeConfig,
    client: &reqwest::Client,
    rt: &Runtime,
    failing: &mut HashMap<String, Failing>,
    hold: Duration,
) -> Result<Option<Duration>> {
    let observed: Vec<ObservedSandbox> = rt.observe()?;
    let (free_mib, psi) = runtime::host_pressure();
    let failures: Vec<CommandFailure> = failing
        .iter()
        .map(|(id, f)| CommandFailure {
            id: id.clone(),
            error: f.error.clone(),
            attempts: f.attempts,
        })
        .collect();

    let req = PollRequest {
        node_id: cfg.node_id.clone(),
        observed,
        free_memory_mib: free_mib,
        pressure_some_avg10: psi,
        hold_ms: Some(hold.as_millis() as u64),
        failures,
    };
    let url = format!("{}/v1/node/poll", cfg.master_url.trim_end_matches('/'));
    let resp: PollResponse = bearer(client.post(&url), cfg)
        .json(&req)
        .send()
        .await?
        .json()
        .await?;

    let now = Instant::now();
    let mut soonest: Option<Duration> = None;
    let mut asked_about: Vec<String> = Vec::new();

    for cmd in resp.commands {
        let id = command_id(&cmd).to_string();
        asked_about.push(id.clone());

        if let Some(f) = failing.get(&id) {
            if f.retry_at > now {
                let wait = f.retry_at - now;
                soonest = Some(soonest.map_or(wait, |s: Duration| s.min(wait)));
                continue;
            }
        }

        // Exec is request/response: a caller is waiting, so it does not go
        // through the failure/backoff path that declarative commands use.
        if let Command::Exec {
            exec_id,
            id: sb,
            args,
            timeout_secs,
        } = &cmd
        {
            let (stdout, stderr, exit_code) =
                match rt.exec(sb, args, Duration::from_secs(*timeout_secs)) {
                    Ok(t) => t,
                    Err(e) => (String::new(), format!("forkd: {e:#}\n"), -1),
                };
            let url = format!(
                "{}/v1/node/exec-result",
                cfg.master_url.trim_end_matches('/')
            );
            let body = ExecResult {
                exec_id: exec_id.clone(),
                stdout,
                stderr,
                exit_code,
            };
            if let Err(e) = bearer(client.post(&url), cfg).json(&body).send().await {
                // The caller will time out. Nothing to retry: it holds an
                // open request, not a queue.
                tracing::warn!(error = %e, exec = %exec_id, "could not report exec result");
            }
            continue;
        }

        let out = match &cmd {
            Command::Create {
                id,
                template,
                spec,
                port,
                volume,
            } => rt.create(id, template, spec, *port, volume.as_deref()),
            Command::SetTier { id, tier } => rt.set_tier(id, *tier),
            Command::Destroy { id } => rt.destroy(id),
            // Handled above; it never reaches here.
            Command::Exec { .. } => unreachable!("exec is handled before this match"),
        };
        match out {
            Ok(()) => {
                failing.remove(&id);
            }
            Err(e) => {
                // One bad command must not stop the others, but it must not
                // be retried flat out either.
                let entry = failing.entry(id.clone()).or_insert(Failing {
                    attempts: 0,
                    retry_at: now,
                    error: String::new(),
                });
                entry.attempts += 1;
                entry.error = format!("{e:#}");
                let wait = backoff_for(entry.attempts);
                entry.retry_at = now + wait;
                soonest = Some(soonest.map_or(wait, |s: Duration| s.min(wait)));
                tracing::warn!(
                    sandbox = %id, attempts = entry.attempts,
                    retry_in_ms = wait.as_millis(), error = %entry.error,
                    "command failed, backing off"
                );
            }
        }
    }

    // Forget sandboxes the master has stopped asking about -- it has
    // quarantined them. Holding on to them would keep shortening the hold
    // and reopening the request for work that will never be issued again,
    // which is the whole cost we are trying not to pay.
    failing.retain(|id, _| asked_about.contains(id));

    // Reopen the request at once either way; this only says how long the
    // master may hold it before we need to come back for a retry.
    Ok(soonest)
}

/// Attach the master's bearer token, when there is one.
fn bearer(rb: reqwest::RequestBuilder, cfg: &NodeConfig) -> reqwest::RequestBuilder {
    match cfg.api_token.as_deref() {
        Some(t) if !t.is_empty() => rb.bearer_auth(t),
        _ => rb,
    }
}

fn command_id(cmd: &Command) -> &str {
    match cmd {
        Command::Create { id, .. }
        | Command::SetTier { id, .. }
        | Command::Destroy { id }
        | Command::Exec { id, .. } => id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_then_caps() {
        assert_eq!(backoff_for(1), Duration::from_millis(1000));
        assert_eq!(backoff_for(2), Duration::from_millis(2000));
        assert_eq!(backoff_for(3), Duration::from_millis(4000));
        // Caps rather than overflowing, however long something stays broken.
        assert_eq!(backoff_for(7), BACKOFF_MAX);
        assert_eq!(backoff_for(64), BACKOFF_MAX);
        assert_eq!(backoff_for(u32::MAX), BACKOFF_MAX);
    }

    /// The hold the node asks for is the sooner of its own refresh interval
    /// and its next due retry -- so it waits by staying parked, and a
    /// backoff on one sandbox never delays dispatch for another.
    #[test]
    fn a_due_retry_shortens_the_hold_rather_than_sleeping() {
        let configured = Duration::from_secs(25);
        let retry_due = Some(Duration::from_millis(500));
        assert_eq!(
            retry_due.map_or(configured, |d: Duration| d.min(configured)),
            Duration::from_millis(500)
        );
        // With nothing due, park for the full interval.
        let none: Option<Duration> = None;
        assert_eq!(
            none.map_or(configured, |d: Duration| d.min(configured)),
            configured
        );
    }
}
