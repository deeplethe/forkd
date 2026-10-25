//! The sandbox agent.
//!
//! Two ways to be driven, sharing everything but the direction of the
//! connection:
//!
//! - **pull** (`serve`): registers once, then polls the master for work. It
//!   never listens, so the machine needs no inbound port -- the case for a
//!   node behind NAT the master cannot reach.
//! - **listen** (`agent::serve_listen`): waits on a port with a fixed
//!   pairing code, and the master connects to it. A node that is already
//!   installed only needs to be pointed at.

mod agent;
mod bundle;
mod gateway;
mod net;
mod runtime;

pub use agent::{serve_listen, Identity as AgentIdentity};
pub use bundle::TemplateSpec;
pub use gateway::{Exposed, Gateway};
pub use net::NetConfig;
pub use runtime::{Runtime, RuntimeConfig};

use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::proto::{
    Command, CommandFailure, CommandResult, ObservedSandbox, PollRequest, PollResponse,
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
    /// Below this much free memory, the node stops starting sandboxes --
    /// whatever desired state says.
    ///
    /// The master has the same two numbers and checks them when it places
    /// a sandbox, but it checks them against what this node last told it,
    /// which is up to one `hold` old, and it reserves nothing. A burst
    /// that arrives inside one of those windows is judged entirely
    /// against the same stale reading.
    ///
    /// Measured: six 2-vCPU / 1.9 GiB nodes reporting ~1500 MiB free were
    /// handed 3000 sandboxes in 2.1 seconds. Every placement passed the
    /// master's 512 MiB floor, because every placement saw the same
    /// 1500 MiB. The nodes ran down to 68 MiB, load 27, and stopped
    /// answering ssh. `host_pressure()` was being read on this side the
    /// whole time -- and used only to fill in the next poll.
    ///
    /// So the node now decides for itself. It is the only process with a
    /// current number, and it is the one that pays.
    pub admit_min_free_mib: u64,
    /// And above this 10-second memory-pressure average. PSI is the
    /// signal that leads free memory: on bm the apparent cost per sandbox
    /// FELL while pressure climbed, because swap was absorbing it.
    pub admit_max_psi: f64,
}

/// Why this node will not start a sandbox right now, if it will not.
///
/// Only for work that COSTS memory. A teardown or a demotion is how a
/// node that is short of memory stops being short of memory; refusing
/// those would turn a busy node into a stuck one.
fn holding_back(cfg: &NodeConfig, cmd: &Command) -> Option<String> {
    holding_back_at(
        cfg.admit_min_free_mib,
        cfg.admit_max_psi,
        cmd,
        runtime::host_pressure(),
    )
}

/// The decision, with the host reading passed in.
///
/// Split out so a test can state the pressure instead of asking the
/// machine it happens to run on -- the same mistake the rootfs sweep
/// tests made with pid 1000.
fn holding_back_at(
    min_free_mib: u64,
    max_psi: f64,
    cmd: &Command,
    (free_mib, psi): (u64, f64),
) -> Option<String> {
    let costs_memory = match cmd {
        Command::Create { .. } => true,
        // Hot is the only tier that has to restore something. Frozen,
        // Snapshot and Detached all give memory back.
        Command::SetTier { tier, .. } => *tier == crate::tier::Tier::Hot,
        Command::Destroy { .. } => false,
        Command::Exec { .. } | Command::Branch { .. } => false,
    };
    if !costs_memory {
        return None;
    }
    // 0 means the read failed, not that the machine is full. Failing open
    // on an unreadable /proc keeps a bad read from wedging the fleet --
    // the same convention the master's `Admission::refuse` uses.
    if free_mib > 0 && free_mib < min_free_mib {
        return Some(format!(
            "node held it back: {free_mib} MiB free, below its own {min_free_mib} MiB floor"
        ));
    }
    if psi > max_psi {
        return Some(format!(
            "node held it back: memory pressure {psi:.1} above its own {max_psi:.1} ceiling"
        ));
    }
    None
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

    // Before any sandbox exists: the namespace alone does not isolate a
    // sandbox from the host's own services, which listen on 0.0.0.0 and so
    // answer on the veth too.
    net::install_rules(&cfg.runtime.net).context("installing the sandbox network rules")?;

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
    let mut builder = reqwest::Client::builder().timeout(cfg.hold + Duration::from_secs(15));
    // Every later call presents this node's certificate, which is how the
    // master knows which node is on the connection. Without it the master
    // would have to believe the node id in the body, and the bearer token
    // is one shared secret -- so any holder could poll as any node and be
    // handed its sandboxes.
    let ca_path = cfg.pki_dir.join("ca.pem");
    if cert_path.exists() && ca_path.exists() {
        crate::tls::install_crypto_provider();
        builder = builder.use_preconfigured_tls(crate::tls::client_config(
            &crate::tls::read_pem(&cert_path)?,
            &crate::tls::read_pem(&key_path)?,
            &crate::tls::read_pem(&ca_path)?,
        )?);
    } else if cfg.master_url.starts_with("https://") {
        bail!(
            "no client certificate at {}; the master requires one over TLS",
            cert_path.display()
        );
    }
    let client = builder.build()?;

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
    // Listeners for the sandboxes exposed off-box. Reconciled on every poll.
    let mut gateway = Gateway::new();
    loop {
        match poll_once(&cfg, &client, &rt, &mut failing, next_hold, &mut gateway).await {
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
    // First contact, and the only request that carries the registration
    // token. The node has no CA certificate yet, so the master is verified
    // by the CA fingerprint the token itself carries; an attacker standing
    // in for the master fails the handshake and never sees the token.
    let (pin, _) = crate::tls::split_token(token);
    let mut builder = reqwest::Client::builder();
    match pin {
        Some(p) => {
            crate::tls::install_crypto_provider();
            builder = builder.use_preconfigured_tls(crate::tls::pinned_client_config(p));
        }
        None => {
            tracing::warn!(
                "this registration token carries no CA fingerprint, so the \
                 master cannot be verified on first contact. Mint a new one \
                 with `forkd-serve node-token` to get a pinned token."
            );
        }
    }
    let resp = builder
        .build()?
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
/// What this node tells whoever drives it: what is running here, how much
/// room is left, and which commands keep failing.
///
/// Shared by both ways a node can be driven. Pulling, it is the body of the
/// poll the node sends; listening, it is the answer to the master's sync.
/// Either way the gateway is reconciled first, so a listener never outlives
/// its sandbox by a round trip.
async fn report(
    cfg: &NodeConfig,
    rt: &Runtime,
    failing: &HashMap<String, Failing>,
    gw: &mut Gateway,
    hold: Option<Duration>,
) -> Result<PollRequest> {
    let observed: Vec<ObservedSandbox> = rt.observe()?;

    // Expose what is here, withdraw what is not. Driven off `observed` for
    // the same reason the rest of the node is: a listener that outlived its
    // sandbox would leave a port open on the host, and one that was never
    // started would leave the sandbox unreachable with nothing saying why.
    // Only serving sandboxes get a listener -- a cold one would answer and
    // then fail to forward, and #327 already refuses its port outright.
    let expose: Vec<Exposed> = observed
        .iter()
        .filter(|o| o.tier.is_serving())
        .filter_map(|o| rt.exposed(&o.id))
        .collect();
    gw.reconcile(&expose).await;
    tracing::debug!(
        count = observed.len(),
        ids = ?observed.iter().map(|o| o.id.as_str()).collect::<Vec<_>>(),
        "observed"
    );
    let (free_mib, psi) = runtime::host_pressure();
    let failures: Vec<CommandFailure> = failing
        .iter()
        .map(|(id, f)| CommandFailure {
            id: id.clone(),
            error: f.error.clone(),
            attempts: f.attempts,
        })
        .collect();

    Ok(PollRequest {
        node_id: cfg.node_id.clone(),
        observed,
        free_memory_mib: free_mib,
        pressure_some_avg10: psi,
        hold_ms: hold.map(|h| h.as_millis() as u64),
        failures,
    })
}

/// What happened when one command was offered to this node.
enum Step {
    /// Applied. The failure book no longer lists it.
    Done,
    /// Failed or still backing off; offer it again after this long.
    Later(Duration),
    /// A request/response command finished, and someone is waiting on it.
    Answered(CommandResult),
}

/// Offer one command. Shared by both ways a node can be driven, so the
/// backoff, the admission check and the failure book behave identically
/// whichever side opened the connection.
fn run_one(
    cfg: &NodeConfig,
    rt: &Runtime,
    failing: &mut HashMap<String, Failing>,
    cmd: &Command,
    now: Instant,
) -> Step {
    let id = command_id(cmd).to_string();

    if let Some(f) = failing.get(&id) {
        if f.retry_at > now {
            return Step::Later(f.retry_at - now);
        }
    }

    // Exec and Branch are request/response: a caller is waiting, so they do
    // not go through the failure/backoff path that declarative commands use.
    match cmd {
        Command::Exec {
            exec_id,
            id: sb,
            args,
            timeout_secs,
        } => {
            return Step::Answered(
                match rt.exec(sb, args, Duration::from_secs(*timeout_secs)) {
                    Ok((stdout, stderr, exit_code)) => CommandResult {
                        result_id: exec_id.clone(),
                        stdout,
                        stderr,
                        exit_code,
                        size_bytes: 0,
                    },
                    Err(e) => CommandResult {
                        result_id: exec_id.clone(),
                        stdout: String::new(),
                        stderr: format!("forkd: {e:#}\n"),
                        exit_code: -1,
                        size_bytes: 0,
                    },
                },
            )
        }
        Command::Branch {
            id: sb,
            tag,
            result_id,
        } => {
            return Step::Answered(match rt.branch(sb, tag) {
                Ok(size_bytes) => CommandResult {
                    result_id: result_id.clone(),
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: 0,
                    size_bytes,
                },
                Err(e) => CommandResult {
                    result_id: result_id.clone(),
                    stdout: String::new(),
                    stderr: format!("forkd: {e:#}\n"),
                    exit_code: -1,
                    size_bytes: 0,
                },
            })
        }
        _ => {}
    }

    // Checked here rather than inside the runtime so that the refusal is
    // reported like any other command failure: the master records it on the
    // sandbox, the panel shows the reason, and the usual backoff retries it
    // in case the pressure passes.
    let out = if let Some(why) = holding_back(cfg, cmd) {
        tracing::warn!(sandbox = %id, reason = %why, "refusing work");
        Err(anyhow::anyhow!(why))
    } else {
        match cmd {
            Command::Create {
                id,
                template,
                spec,
                port,
                volume,
                net_index,
                from_tag,
                gateway_port,
                token,
            } => rt.create(&runtime::CreateRequest {
                id,
                template,
                spec,
                port: *port,
                volume: volume.as_deref(),
                net_index: *net_index,
                from_tag: from_tag.as_deref(),
                gateway_port: *gateway_port,
                token,
            }),
            Command::SetTier { id, tier } => rt.set_tier(id, *tier),
            Command::Destroy { id, net_index } => rt.destroy_with_net(id, *net_index),
            Command::Exec { .. } | Command::Branch { .. } => {
                unreachable!("awaited commands are answered above")
            }
        }
    };
    match out {
        Ok(()) => {
            failing.remove(&id);
            Step::Done
        }
        Err(e) => {
            // One bad command must not stop the others, but it must not be
            // retried flat out either.
            let entry = failing.entry(id.clone()).or_insert(Failing {
                attempts: 0,
                retry_at: now,
                error: String::new(),
            });
            entry.attempts += 1;
            entry.error = format!("{e:#}");
            let wait = backoff_for(entry.attempts);
            entry.retry_at = now + wait;
            tracing::warn!(
                sandbox = %id, attempts = entry.attempts,
                retry_in_ms = wait.as_millis(), error = %entry.error,
                "command failed, backing off"
            );
            Step::Later(wait)
        }
    }
}

fn sooner(a: Option<Duration>, b: Duration) -> Option<Duration> {
    Some(a.map_or(b, |a| a.min(b)))
}

async fn poll_once(
    cfg: &NodeConfig,
    client: &reqwest::Client,
    rt: &Runtime,
    failing: &mut HashMap<String, Failing>,
    hold: Duration,
    gw: &mut Gateway,
) -> Result<Option<Duration>> {
    let req = report(cfg, rt, failing, gw, Some(hold)).await?;
    let url = format!("{}/v1/node/poll", cfg.master_url.trim_end_matches('/'));
    let raw = bearer(client.post(&url), cfg).json(&req).send().await?;
    // Check the status before parsing. Deserialising straight into
    // PollResponse turned every refusal into "error decoding response body"
    // and an endless backoff: on bm a revoked node retried forever without
    // ever saying it had been revoked.
    let status = raw.status();
    if !status.is_success() {
        let body = raw.text().await.unwrap_or_default();
        let reason = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v["error"].as_str().map(str::to_string))
            .unwrap_or(body);
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            // Not a transient failure: retrying cannot fix a credential the
            // master has rejected, so say so plainly and keep saying it
            // rather than burying it in a backoff log line.
            tracing::error!(
                status = %status.as_u16(),
                node = %cfg.node_id,
                "the master refused this node: {reason}"
            );
        }
        bail!("master refused the poll: {} {reason}", status.as_u16());
    }
    let resp: PollResponse = raw.json().await?;

    let now = Instant::now();
    let mut soonest: Option<Duration> = None;
    let mut asked_about: Vec<String> = Vec::new();
    for cmd in &resp.commands {
        asked_about.push(command_id(cmd).to_string());
        match run_one(cfg, rt, failing, cmd, now) {
            Step::Done => {}
            Step::Later(wait) => soonest = sooner(soonest, wait),
            // Posted as soon as it exists, not at the end of the batch, so a
            // caller waiting on a fast exec is not held up by a slow create
            // later in the same response.
            Step::Answered(body) => {
                let url = format!("{}/v1/node/result", cfg.master_url.trim_end_matches('/'));
                if let Err(e) = bearer(client.post(&url), cfg).json(&body).send().await {
                    // The caller will time out. Nothing to retry: it holds an
                    // open request, not a queue.
                    tracing::warn!(error = %e, result = %body.result_id, "could not report a result");
                }
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
        | Command::Destroy { id, .. }
        | Command::Branch { id, .. }
        | Command::Exec { id, .. } => id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::tier::Tier;

    fn create(id: &str) -> Command {
        Command::Create {
            id: id.into(),
            template: "py".into(),
            spec: Default::default(),
            port: 8080,
            volume: None,
            net_index: None,
            from_tag: None,
            gateway_port: None,
            token: String::new(),
        }
    }

    /// The node stops starting sandboxes on its own reading, without
    /// waiting to be told. The master's copy of this number is up to one
    /// hold old and nothing is reserved against it, so a burst inside one
    /// window is placed entirely against a figure that was true before
    /// the burst began.
    #[test]
    fn a_node_short_of_memory_refuses_to_start_more() {
        let why = holding_back_at(512, 8.0, &create("sb1"), (172, 0.0));
        assert!(why.as_deref().unwrap().contains("172 MiB free"), "{why:?}");
        assert!(holding_back_at(512, 8.0, &create("sb1"), (1500, 0.0)).is_none());
    }

    #[test]
    fn pressure_refuses_even_with_memory_apparently_free() {
        // The case free memory misses: swap absorbing the cost while the
        // machine grinds.
        let why = holding_back_at(512, 8.0, &create("sb1"), (2000, 20.0));
        assert!(why.as_deref().unwrap().contains("pressure 20.0"), "{why:?}");
    }

    /// The one that matters most. A node under pressure gets out of it by
    /// destroying sandboxes and demoting them; refusing that work would
    /// turn a busy node into a permanently stuck one.
    #[test]
    fn work_that_frees_memory_is_never_refused() {
        let hopeless = (1, 99.0);
        for cmd in [
            Command::Destroy {
                id: "sb1".into(),
                net_index: Some(3),
            },
            Command::SetTier {
                id: "sb1".into(),
                tier: Tier::Frozen,
            },
            Command::SetTier {
                id: "sb1".into(),
                tier: Tier::Snapshot,
            },
            Command::SetTier {
                id: "sb1".into(),
                tier: Tier::Detached,
            },
        ] {
            assert!(
                holding_back_at(512, 8.0, &cmd, hopeless).is_none(),
                "{cmd:?} must go ahead however bad the pressure"
            );
        }
        // Hot is the exception: it restores something, so it costs.
        assert!(holding_back_at(
            512,
            8.0,
            &Command::SetTier {
                id: "sb1".into(),
                tier: Tier::Hot
            },
            hopeless
        )
        .is_some());
    }

    /// An unreadable /proc reads as 0, which must not be mistaken for a
    /// full machine -- that would wedge every node on one bad read.
    #[test]
    fn an_unreadable_meminfo_fails_open() {
        assert!(holding_back_at(512, 8.0, &create("sb1"), (0, 0.0)).is_none());
    }

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
