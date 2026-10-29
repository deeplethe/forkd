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
mod backend;
mod bundle;
mod firecracker;
mod gateway;
mod host;
mod net;
mod reclaim;
mod runtime;
mod tags;
mod wake;

pub use agent::{serve_listen, Identity as AgentIdentity};
pub use backend::{Backend, Runsc};
pub use bundle::TemplateSpec;
pub use gateway::{Exposed, Gateway};
pub use host::{
    bootstrap_compressed_swap, compressed_swap, decide_compression, decide_reclaim, pin_in_zswap,
    Lever,
};
pub use net::NetConfig;
pub use runtime::{BackendKind, Runtime, RuntimeConfig, WARM_SEED_PREFIX};
pub use tags::Fetches;
pub use wake::Wakeups;

use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::proto::{
    Command, CommandFailure, CommandResult, ObservedSandbox, PollRequest, PollResponse,
    RegisterRequest, RegisterResponse,
};
use crate::tier::Tier;

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
    /// The last attempt was held back for want of memory, not failed.
    held_back: bool,
}

const BACKOFF_MIN: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
/// The longest a held-back command waits before the node looks again.
/// Looking is a read of /proc, and memory comes back as soon as something
/// idle goes to disk, so a minute's backoff only delays the start.
const HELD_BACK_RETRY: Duration = Duration::from_secs(5);

/// A command this node would not run for want of memory. Distinct from a
/// failure so that it can be reported as one that will go ahead once there
/// is room, rather than counted towards giving up on the sandbox.
#[derive(Debug)]
pub struct HeldBack(pub String);

impl std::fmt::Display for HeldBack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for HeldBack {}

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
    /// Which density levers are on here, for the master to show per node.
    /// Decided at startup (`host.rs`), so fixed for the process's life.
    pub features: Vec<String>,
    /// And above this 10-second memory-pressure average. PSI is the
    /// signal that leads free memory: on bm the apparent cost per sandbox
    /// FELL while pressure climbed, because swap was absorbing it.
    pub admit_max_psi: f64,
    /// The pressure ceiling for a wake. A wake is a caller waiting on a
    /// sandbox that exists, and it has already made room under the floor;
    /// whether it should also give way to pressure as readily as a create
    /// is what this lets a node decide.
    pub wake_max_psi: f64,
    /// How long a wake may take to start; see `wake::WAKE_BUDGET`.
    pub wake_budget: Duration,
    /// How many wakes may wait to restore at once; `None` for twice the
    /// restore permits.
    pub wake_max_waiting: Option<usize>,
    /// Tags being fetched from other nodes, so one fetch serves every child
    /// of a tag that arrives while it runs.
    pub fetches: Arc<Fetches>,
}

/// How soon to look again at a fork whose image is still being fetched.
const FETCH_RECHECK: Duration = Duration::from_millis(500);

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
        runtime::host_reading(),
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
    reading: impl Into<runtime::HostReading>,
) -> Option<String> {
    let reading = reading.into();
    let costs_memory = match cmd {
        Command::Create { .. } => true,
        // Hot is the only tier that has to restore something. Frozen,
        // Snapshot and Detached all give memory back.
        Command::SetTier { tier, .. } => *tier == crate::tier::Tier::Hot,
        Command::Destroy { .. } | Command::DropTag { .. } => false,
        Command::Exec { .. } | Command::Branch { .. } => false,
    };
    if !costs_memory {
        return None;
    }
    if let Some(why) = short_of(min_free_mib, &reading) {
        return Some(why);
    }
    let psi = reading.psi;
    if psi > max_psi {
        return Some(format!(
            "node held it back: memory pressure {psi:.1} above its own {max_psi:.1} ceiling"
        ));
    }
    None
}

/// Under the floor: too little memory, or too little swap to squeeze
/// anything more into. What a wake makes room for by putting sandboxes to
/// disk, which gives back both.
fn short_of(min_free_mib: u64, reading: &runtime::HostReading) -> Option<String> {
    // 0 means the read failed, not that the machine is full. Failing open
    // on an unreadable /proc keeps a bad read from wedging the fleet --
    // the same convention the master's `Admission::refuse` uses.
    let free_mib = reading.free_mib;
    if free_mib > 0 && free_mib < min_free_mib {
        return Some(format!(
            "node held it back: {free_mib} MiB free, below its own {min_free_mib} MiB floor"
        ));
    }
    if reading.swap_short(min_free_mib) {
        let floor = reading.swap_floor_mib(min_free_mib).unwrap_or_default();
        return Some(format!(
            "node held it back: {} MiB of swap free, below its own {floor} MiB floor",
            reading.swap_free_mib
        ));
    }
    None
}

/// What the gateway calls to wake a sandbox: the same promotion, room made
/// the same way, as one the master asks for.
fn wakeups(cfg: &NodeConfig, rt: &Runtime) -> Arc<Wakeups> {
    let rt = rt.clone();
    let (min_free, max_psi) = (cfg.admit_min_free_mib, cfg.wake_max_psi);
    Arc::new(
        Wakeups::new(move |w, id, patience| promote(min_free, max_psi, &rt, w, id, patience))
            .with_limits(cfg.wake_budget, cfg.wake_max_waiting),
    )
}

/// How often a waiting wake reads the pressure again.
const PRESSURE_POLL: Duration = Duration::from_millis(250);

/// How many idle sandboxes one promotion may put to disk to make room.
/// Each is a checkpoint, a few hundred milliseconds on a 2-vCPU node, and a
/// caller is waiting on all of them.
const MAX_EVICTIONS: usize = 4;

/// Bring a sandbox to Hot. If the node is under its memory floor, put the
/// longest-idle sandboxes here to disk first, until it is not.
///
/// Before this a wake under the floor was refused. Measured on 2026-10-26,
/// six 2-vCPU nodes with 40 sandboxes each where about 12 fit awake: 13% of
/// requests were refused at 5 req/s and a third at 40, every one of them
/// "below its own 512 MiB floor", while frozen sandboxes nobody had called
/// for minutes held the memory. A wake that waits for a checkpoint is
/// slow; a 503 is a failure.
///
/// Only the floor is made room for. Memory pressure is a 10-second
/// average that does not move when a checkpoint finishes, so evicting
/// until it did would empty the node.
fn promote(
    min_free: u64,
    max_psi: f64,
    rt: &Runtime,
    wake: &Wakeups,
    id: &str,
    patience: Duration,
) -> Result<()> {
    promote_reading(
        min_free,
        max_psi,
        rt,
        wake,
        id,
        patience,
        &runtime::host_reading,
    )
}

/// `promote`, with the pressure reading passed in so a test can state it.
fn promote_reading<R: Into<runtime::HostReading>>(
    min_free: u64,
    max_psi: f64,
    rt: &Runtime,
    wake: &Wakeups,
    id: &str,
    patience: Duration,
    read: &dyn Fn() -> R,
) -> Result<()> {
    let hot = Command::SetTier {
        id: id.to_string(),
        tier: Tier::Hot,
    };
    let deadline = Instant::now() + patience;
    let mut evicted = 0;
    loop {
        let reading = read().into();
        let Some(why) = holding_back_at(min_free, max_psi, &hot, reading) else {
            return rt.set_tier(id, Tier::Hot);
        };
        let under_floor = short_of(min_free, &reading).is_some();
        if under_floor && evicted < MAX_EVICTIONS && evict_one(rt, wake, id)? {
            evicted += 1;
            continue;
        }
        // Pressure passes on its own; a floor nothing more can be put away
        // for does not.
        if !under_floor && Instant::now() < deadline {
            std::thread::sleep(PRESSURE_POLL);
            continue;
        }
        tracing::warn!(sandbox = %id, reason = %why, evicted, "refusing work");
        return Err(HeldBack(why).into());
    }
}

/// How long a bundle may sit with nothing running in it before the node
/// takes it for dead. A start or a restore in progress holds the sandbox's
/// lock, which the reaper never waits for, so this only has to outlast the
/// moment between writing a bundle and starting it.
const DEAD_AFTER: Duration = Duration::from_secs(60);

/// Clear away sandboxes that have died, network and all.
///
/// The master never hears of one -- `observe` leaves it out -- so it cannot
/// ask for it to be destroyed, and the slot it held is handed on while its
/// veth still answers for the address. Once it is gone, a sandbox the
/// master still wants is simply not here, and the next diff creates it
/// again: a crashed sandbox restarts, from its template.
fn reap_dead(rt: &Runtime, wake: &Wakeups) {
    for id in rt.dead(DEAD_AFTER) {
        let lock = wake.lock(&id);
        let Some(_held) = lock.try_lock() else {
            continue;
        };
        match rt.destroy_with_net(&id, None) {
            Ok(()) => tracing::warn!(sandbox = %id, "cleared away a sandbox that had died"),
            Err(e) => {
                tracing::warn!(sandbox = %id, error = %format!("{e:#}"), "could not clear away a sandbox that had died")
            }
        }
        drop(_held);
        wake.forget(&id);
    }
}

/// Put the longest-idle sandbox here other than `except` to disk. False
/// when there is none that can go.
fn evict_one(rt: &Runtime, wake: &Wakeups, except: &str) -> Result<bool> {
    for victim in wake.victims(&rt.observe()?, except, Instant::now()) {
        // One whose tier is already changing -- being woken, or demoted by
        // the master -- is skipped, not waited for: two wakes each holding
        // their own sandbox's lock and waiting on the other's would never
        // finish.
        let lock = wake.lock(&victim);
        let Some(_held) = lock.try_lock() else {
            continue;
        };
        // The list was drawn before the lock was had: a request may have
        // reached this one, or a wake brought it up, in between.
        if wake.protected(&victim, Instant::now()) {
            continue;
        }
        let t0 = Instant::now();
        match rt.set_tier(&victim, Tier::Snapshot) {
            Ok(()) => {
                wake.put_away(&victim);
                tracing::info!(
                    sandbox = %victim, waking = %except, ms = t0.elapsed().as_millis() as u64,
                    "put an idle sandbox to disk to make room for a wake"
                );
                return Ok(true);
            }
            Err(e) => tracing::warn!(
                sandbox = %victim, error = %format!("{e:#}"),
                "could not put an idle sandbox away to make room; trying the next"
            ),
        }
    }
    Ok(false)
}

pub async fn serve(mut cfg: NodeConfig) -> Result<()> {
    std::fs::create_dir_all(&cfg.pki_dir)?;
    // First, because registering reports the backend's fingerprint. This
    // only makes directories; no sandbox is touched until the rules below.
    let rt = Runtime::new(cfg.runtime.clone())?;
    let cert_path = cfg.pki_dir.join("client.pem");
    let key_path = cfg.pki_dir.join("client-key.pem");
    let api_token_path = cfg.pki_dir.join("api-token");

    if !cert_path.exists() || !key_path.exists() {
        let Some(token) = cfg.token.as_deref() else {
            bail!(
                "node has no client certificate and no registration token; \
                 get one from the master with `sandstorm node-token create`"
            );
        };
        let learned = register(&cfg, token, &rt.fingerprint(), &cert_path, &key_path).await?;
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

    tags::clear_partial(&rt.tags_dir());
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
    let wake = wakeups(&cfg, &rt);
    start_workers(&cfg, &rt, &wake);
    let mut gateway = Gateway::waking(wake.clone());
    tokio::spawn(report_wakes(
        cfg.node_id.clone(),
        format!("{}/v1/node/traffic", cfg.master_url.trim_end_matches('/')),
        cfg.api_token.clone(),
        client.clone(),
        rt.clone(),
        wake.clone(),
    ));
    loop {
        match poll_once(
            &cfg,
            &client,
            &rt,
            &mut failing,
            next_hold,
            &mut gateway,
            &wake,
        )
        .await
        {
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

/// The node's background workers, the same whichever way it is driven.
fn start_workers(cfg: &NodeConfig, rt: &Runtime, wake: &Arc<Wakeups>) {
    if cfg.runtime.reclaim_hot {
        reclaim::start(rt.clone(), cfg.admit_max_psi);
    }
    if cfg.runtime.prefetch_snapshots {
        tokio::spawn(prefetch(rt.clone(), wake.clone()));
    }
}

/// Warm the page cache with the snapshot images of sandboxes about to be
/// called, one at a time so a prediction never competes with a real wake
/// for the disk more than it has to.
async fn prefetch(rt: Runtime, wake: Arc<Wakeups>) {
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        for id in wake.due_prefetch(Instant::now()) {
            let rt = rt.clone();
            let who = id.clone();
            let t0 = Instant::now();
            match tokio::task::spawn_blocking(move || rt.prefetch_snapshot(&who)).await {
                Ok(Ok(Some(bytes))) => tracing::info!(
                    %id, mib = bytes >> 20, ms = t0.elapsed().as_millis() as u64,
                    "prefetched the snapshot ahead of its predicted request"
                ),
                Ok(Ok(None)) => {}
                Ok(Err(e)) => tracing::debug!(%id, error = %e, "prefetch failed"),
                Err(_) => {}
            }
        }
    }
}

/// Tell the master about a wake as soon as it happens, instead of on the
/// next poll -- which, with the poll parked, can be a whole hold away.
///
/// Best effort: if this is lost the traffic still goes out with the next
/// poll's report, which is where it went before this existed.
async fn report_wakes(
    node_id: String,
    url: String,
    api_token: Option<String>,
    client: reqwest::Client,
    rt: Runtime,
    wake: Arc<Wakeups>,
) {
    loop {
        wake.woke.notified().await;
        let observed = {
            let rt = rt.clone();
            match tokio::task::spawn_blocking(move || rt.observe()).await {
                Ok(Ok(o)) => o,
                _ => continue,
            }
        };
        let (free_memory_mib, pressure_some_avg10) = runtime::host_pressure();
        let req = PollRequest {
            node_id: node_id.clone(),
            observed,
            free_memory_mib,
            pressure_some_avg10,
            hold_ms: None,
            failures: Vec::new(),
            traffic: wake.take_traffic(),
            evicted: wake.recently_evicted(Instant::now()),
            features: Vec::new(),
            tags: Vec::new(),
        };
        let mut rb = client
            .post(&url)
            .timeout(Duration::from_secs(10))
            .json(&req);
        if let Some(t) = api_token.as_deref().filter(|t| !t.is_empty()) {
            rb = rb.bearer_auth(t);
        }
        if let Err(e) = rb.send().await.and_then(|r| r.error_for_status()) {
            tracing::debug!(error = %e, "could not report a wake early; it goes with the next poll");
        }
    }
}

/// Returns the API token the master handed back, if any.
async fn register(
    cfg: &NodeConfig,
    token: &str,
    fingerprint: &str,
    cert_path: &PathBuf,
    key_path: &PathBuf,
) -> Result<String> {
    let req = RegisterRequest {
        token: token.to_string(),
        node_id: cfg.node_id.clone(),
        fingerprint: fingerprint.to_string(),
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
                 with `sandstormd node-token` to get a pinned token."
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
    wake: &Wakeups,
    hold: Option<Duration>,
) -> Result<PollRequest> {
    reap_dead(rt, wake);
    let observed: Vec<ObservedSandbox> = rt.observe()?;

    // Expose what is here, withdraw what is not. Driven off `observed` for
    // the same reason the rest of the node is: a listener that outlived its
    // sandbox would leave a port open on the host, and one that was never
    // started would leave the sandbox unreachable with nothing saying why.
    // Every tier on this node gets one, cold ones included: a request for a
    // frozen or snapshotted sandbox is what wakes it. Detached has nothing
    // on this node and is not observed, so its port closes.
    let expose: Vec<Exposed> = observed.iter().filter_map(|o| rt.exposed(&o.id)).collect();
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
            held_back: f.held_back,
        })
        .collect();

    Ok(PollRequest {
        node_id: cfg.node_id.clone(),
        observed,
        free_memory_mib: free_mib,
        pressure_some_avg10: psi,
        hold_ms: hold.map(|h| h.as_millis() as u64),
        failures,
        traffic: wake.take_traffic(),
        evicted: wake.recently_evicted(Instant::now()),
        features: cfg.features.clone(),
        tags: tags::held(&rt.tags_dir(), tags::REPORT_MIN_AGE),
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
    wake: &Wakeups,
    cmd: &Command,
    now: Instant,
) -> Step {
    let id = command_id(cmd).to_string();

    if let Some(f) = failing.get(&id) {
        if f.retry_at > now {
            return Step::Later(f.retry_at - now);
        }
    }

    // A sandbox that just served a request is not demoted on a view of it
    // that predates the request. The master issued this before it heard
    // about the traffic; once it has, it stops asking. Not a failure, so
    // the failure book is left alone.
    if let Command::SetTier { tier, .. } = cmd {
        if !tier.is_serving() {
            if let Some(left) = wake.in_grace(&id, now) {
                tracing::debug!(sandbox = %id, %tier, wait_ms = left.as_millis() as u64, "in use; demotion deferred");
                return Step::Later(left);
            }
        }
        // And the other way: a sandbox this node put to disk to make room
        // is not brought back on a view of it that predates that. The
        // master stops asking once a report has told it.
        if *tier < Tier::Snapshot {
            if let Some(left) = wake.evicted_for(&id, now) {
                tracing::debug!(sandbox = %id, %tier, wait_ms = left.as_millis() as u64, "put away to make room; restore deferred");
                return Step::Later(left);
            }
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
                        stderr: format!("sandstorm: {e:#}\n"),
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
                    stderr: format!("sandstorm: {e:#}\n"),
                    exit_code: -1,
                    size_bytes: 0,
                },
            })
        }
        // Housekeeping the master asks for, not work on a sandbox: nothing
        // waits on it and a failure is retried by the next report, which
        // will still list the tag.
        Command::DropTag { tag } => {
            if let Err(e) = rt.drop_tag(tag) {
                tracing::warn!(%tag, error = %format!("{e:#}"), "could not drop a tag");
            }
            return Step::Done;
        }
        _ => {}
    }

    // Checked here rather than inside the runtime so that the refusal is
    // reported like any other command failure: the master records it on the
    // sandbox, the panel shows the reason, and the usual backoff retries it
    // in case the pressure passes.
    // Held for the tier change, so a wake from the gateway cannot run
    // runsc on this sandbox at the same time.
    let lock = wake.lock(&id);
    let _held = lock.lock();
    let out = if let Command::SetTier {
        id,
        tier: Tier::Hot,
    } = cmd
    {
        // No waiting here: this runs on the loop that applies every
        // command, and a held-back command is retried on its own.
        promote(
            cfg.admit_min_free_mib,
            cfg.wake_max_psi,
            rt,
            wake,
            id,
            Duration::ZERO,
        )
    } else if let Some(why) = holding_back(cfg, cmd) {
        tracing::warn!(sandbox = %id, reason = %why, "refusing work");
        Err(HeldBack(why).into())
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
                fetch,
            } => {
                // A fork of a tag baked on another node: its image has to be
                // here before anything is set up for the sandbox. The fetch
                // runs on its own; until it lands, offer the command again
                // shortly, without counting a failure.
                let image = match (from_tag.as_deref(), fetch) {
                    (Some(tag), Some(from)) => cfg.fetches.ensure(&rt.tags_dir(), tag, from),
                    _ => tags::Status::Ready,
                };
                match image {
                    tags::Status::Pending => return Step::Later(FETCH_RECHECK),
                    tags::Status::Failed(e) => Err(anyhow::anyhow!(e)),
                    tags::Status::Ready => rt.create(&runtime::CreateRequest {
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
                }
            }
            Command::SetTier { id, tier } => rt.set_tier(id, *tier),
            Command::Destroy { id, net_index } => rt.destroy_with_net(id, *net_index),
            Command::Exec { .. } | Command::Branch { .. } | Command::DropTag { .. } => {
                unreachable!("answered above")
            }
        }
    };
    match out {
        Ok(()) => {
            failing.remove(&id);
            if matches!(
                cmd,
                Command::Create { .. }
                    | Command::SetTier {
                        tier: Tier::Hot,
                        ..
                    }
            ) {
                wake.started(&id);
            }
            if matches!(cmd, Command::Destroy { .. }) {
                drop(_held);
                wake.forget(&id);
            }
            Step::Done
        }
        Err(e) => {
            // One bad command must not stop the others, but it must not be
            // retried flat out either.
            let entry = failing.entry(id.clone()).or_insert(Failing {
                attempts: 0,
                retry_at: now,
                error: String::new(),
                held_back: false,
            });
            entry.attempts += 1;
            entry.error = format!("{e:#}");
            entry.held_back = e.downcast_ref::<HeldBack>().is_some();
            let wait = if entry.held_back {
                backoff_for(entry.attempts).min(HELD_BACK_RETRY)
            } else {
                backoff_for(entry.attempts)
            };
            entry.retry_at = now + wait;
            if !entry.held_back {
                tracing::warn!(
                    sandbox = %id, attempts = entry.attempts,
                    retry_in_ms = wait.as_millis(), error = %entry.error,
                    "command failed, backing off"
                );
            }
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
    wake: &Wakeups,
) -> Result<Option<Duration>> {
    let req = report(cfg, rt, failing, gw, wake, Some(hold)).await?;
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
        match run_one(cfg, rt, failing, wake, cmd, now) {
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
        Command::DropTag { tag } => tag,
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
            fetch: None,
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

    fn with_swap(free_mib: u64, swap_free_mib: u64, swap_total_mib: u64) -> runtime::HostReading {
        runtime::HostReading {
            free_mib,
            swap_free_mib,
            swap_total_mib,
            psi: 0.0,
        }
    }

    /// f8 on 2026-10-28: 678 MiB available, swap gone. Pages squeezed into
    /// zswap cost little RAM but a slot each, so memory alone said yes.
    #[test]
    fn a_node_out_of_swap_refuses_however_free_memory_looks() {
        let why = holding_back_at(512, 8.0, &create("sb1"), with_swap(1500, 100, 2048));
        let why = why.unwrap();
        assert!(why.contains("100 MiB of swap free"), "{why}");
        assert!(why.contains("512 MiB floor"), "{why}");
        assert!(holding_back_at(512, 8.0, &create("sb1"), with_swap(1500, 900, 2048)).is_none());
    }

    /// A quarter of a small swap, not the whole memory floor: a node with
    /// 256 MiB of swap must not be short for ever.
    #[test]
    fn a_small_swap_keeps_a_smaller_floor() {
        assert!(holding_back_at(512, 8.0, &create("sb1"), with_swap(1500, 100, 256)).is_none());
        assert!(holding_back_at(512, 8.0, &create("sb1"), with_swap(1500, 40, 256)).is_some());
        // No swap at all: nothing to run short of.
        assert!(holding_back_at(512, 8.0, &create("sb1"), with_swap(1500, 0, 0)).is_none());
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

#[cfg(test)]
mod wake_tests {
    use super::*;
    use crate::tier::Tier;

    fn node(dir: &std::path::Path) -> (NodeConfig, Runtime) {
        let runtime = RuntimeConfig {
            work_dir: dir.join("work"),
            volume_root: dir.join("volumes"),
            ..Default::default()
        };
        let cfg = NodeConfig {
            node_id: "n1".into(),
            master_url: "http://127.0.0.1:1".into(),
            token: None,
            api_token: None,
            pki_dir: dir.join("pki"),
            runtime: runtime.clone(),
            hold: Duration::from_secs(25),
            admit_min_free_mib: 0,
            admit_max_psi: f64::MAX,
            wake_max_psi: f64::MAX,
            wake_budget: wake::WAKE_BUDGET,
            wake_max_waiting: None,
            features: Vec::new(),
            fetches: Default::default(),
        };
        (cfg, Runtime::new(runtime).unwrap())
    }

    fn set_tier(tier: Tier) -> Command {
        Command::SetTier {
            id: "sb".into(),
            tier,
        }
    }

    /// A demotion issued before the master heard about a request must not
    /// undo the wake that request caused. It is deferred, not failed: the
    /// failure book stays empty, so nothing shows up as an error and the
    /// master's quarantine count does not move.
    #[test]
    fn a_sandbox_in_use_is_not_demoted() {
        let dir = tempfile::tempdir().unwrap();
        let (cfg, rt) = node(dir.path());
        let w = Wakeups::new(|_, _, _| Ok(()));
        let mut failing = HashMap::new();
        w.saw("sb");

        for tier in [Tier::Frozen, Tier::Snapshot, Tier::Detached] {
            match run_one(&cfg, &rt, &mut failing, &w, &set_tier(tier), Instant::now()) {
                Step::Later(d) => {
                    assert!(d > wake::GRACE - Duration::from_secs(1), "{tier}: {d:?}")
                }
                _ => panic!("{tier}: demoted a sandbox that just served a request"),
            }
        }
        assert!(failing.is_empty(), "a deferral is not a failure");

        // Promotion is never deferred. This one reaches the runtime, which
        // fails it -- there is no such sandbox here -- and that is the
        // evidence it was attempted.
        run_one(
            &cfg,
            &rt,
            &mut failing,
            &w,
            &set_tier(Tier::Hot),
            Instant::now(),
        );
        assert!(failing.contains_key("sb"));

        // And once the grace is over, demotion goes ahead (to the same
        // runtime failure).
        failing.clear();
        let later = Instant::now() + wake::GRACE;
        run_one(
            &cfg,
            &rt,
            &mut failing,
            &w,
            &set_tier(Tier::Snapshot),
            later,
        );
        assert!(failing.contains_key("sb"));
    }

    /// A bundle with nothing running and no snapshot is dead once it is old
    /// enough; one on disk, a warm seed, or a young one is not.
    #[test]
    fn a_sandbox_that_died_is_found() {
        let dir = tempfile::tempdir().unwrap();
        let (cfg, rt) = node(dir.path());
        let bundles = cfg.runtime.work_dir.join("bundles");
        let seed = format!("{}py", runtime::WARM_SEED_PREFIX);
        for id in ["sb-dead", "sb-on-disk", seed.as_str()] {
            std::fs::create_dir_all(bundles.join(id)).unwrap();
        }
        std::fs::create_dir_all(cfg.runtime.work_dir.join("snapshots").join("sb-on-disk")).unwrap();
        assert_eq!(rt.dead(Duration::ZERO), ["sb-dead"]);
        assert!(
            rt.dead(Duration::from_secs(3600)).is_empty(),
            "too young to call dead"
        );
    }

    /// Short of swap is a floor, not pressure: it does not pass on its own,
    /// so a wake makes room for it or is refused at once rather than
    /// waiting out its patience.
    #[test]
    fn a_wake_short_of_swap_is_not_waited_out() {
        let dir = tempfile::tempdir().unwrap();
        let (_, rt) = node(dir.path());
        let w = Wakeups::new(|_, _, _| Ok(()));
        let t0 = Instant::now();
        let r = promote_reading(512, 8.0, &rt, &w, "sb", Duration::from_secs(5), &|| {
            runtime::HostReading {
                free_mib: 1500,
                swap_free_mib: 100,
                swap_total_mib: 2048,
                psi: 0.0,
            }
        });
        let e = r.unwrap_err();
        assert!(e.downcast_ref::<HeldBack>().is_some(), "{e:#}");
        assert!(format!("{e:#}").contains("of swap free"), "{e:#}");
        assert!(t0.elapsed() < Duration::from_millis(200));
    }

    /// A wake waits out memory pressure rather than being refused for it,
    /// and gives up only once its patience is spent.
    #[test]
    fn a_wake_waits_for_pressure_to_pass() {
        let dir = tempfile::tempdir().unwrap();
        let (_, rt) = node(dir.path());
        let w = Wakeups::new(|_, _, _| Ok(()));
        let held = |r: &Result<()>| {
            r.as_ref()
                .err()
                .is_some_and(|e| e.downcast_ref::<HeldBack>().is_some())
        };

        // Over the ceiling for three readings, then under it: the wake goes
        // ahead, as far as the runtime -- which has no such sandbox.
        let reads = std::cell::Cell::new(0);
        let easing = || {
            reads.set(reads.get() + 1);
            (2000, if reads.get() <= 3 { 20.0 } else { 1.0 })
        };
        let r = promote_reading(512, 8.0, &rt, &w, "sb", Duration::from_secs(5), &easing);
        assert!(!held(&r), "refused a wake the pressure let through: {r:?}");
        assert_eq!(reads.get(), 4);

        // Pressure that never eases is refused once patience runs out.
        let t0 = Instant::now();
        let r = promote_reading(512, 8.0, &rt, &w, "sb", Duration::from_millis(400), &|| {
            (2000, 20.0)
        });
        assert!(held(&r), "{r:?}");
        assert!(t0.elapsed() >= Duration::from_millis(400));

        // And with no patience at all, at once.
        let t0 = Instant::now();
        let r = promote_reading(512, 8.0, &rt, &w, "sb", Duration::ZERO, &|| (2000, 20.0));
        assert!(held(&r));
        assert!(t0.elapsed() < Duration::from_millis(200));
    }

    /// A create the node holds back for want of memory is waiting, not
    /// failing: it is reported as held back, and however many times it is
    /// held back the node looks again within HELD_BACK_RETRY rather than
    /// backing off towards a minute.
    #[test]
    fn a_held_back_create_is_waiting_not_failing() {
        let dir = tempfile::tempdir().unwrap();
        let (mut cfg, rt) = node(dir.path());
        // Any pressure reading, 0 included, is over this.
        cfg.admit_max_psi = -1.0;
        let w = Wakeups::new(|_, _, _| Ok(()));
        let mut failing = HashMap::new();
        let mut create = fork_from_elsewhere("sb", "127.0.0.1:1");
        if let Command::Create {
            fetch, from_tag, ..
        } = &mut create
        {
            *fetch = None;
            *from_tag = None;
        }
        let mut now = Instant::now();
        for _ in 0..10 {
            match run_one(&cfg, &rt, &mut failing, &w, &create, now) {
                Step::Later(d) => assert!(d <= HELD_BACK_RETRY, "waited {d:?}"),
                _ => panic!("a held-back create went ahead"),
            }
            now += HELD_BACK_RETRY;
        }
        let f = &failing["sb"];
        assert!(f.held_back, "{}", f.error);
        assert!(f.error.contains("held it back"), "{}", f.error);
    }

    fn fork_from_elsewhere(id: &str, address: &str) -> Command {
        Command::Create {
            id: id.into(),
            template: "py".into(),
            spec: Default::default(),
            port: 8080,
            volume: None,
            net_index: Some(1),
            from_tag: Some("t".into()),
            gateway_port: None,
            token: String::new(),
            fetch: Some(Box::new(crate::proto::FetchFrom {
                node_id: "src".into(),
                address: address.into(),
                cert_sha256: "00".repeat(32),
                for_node: "n1".into(),
                expires: i64::MAX,
                token: "00".into(),
            })),
        }
    }

    /// While a fork's image is on its way the command is offered again
    /// shortly, and that is not a failure. A fetch that fails is.
    #[tokio::test]
    async fn a_fork_waits_for_its_image_and_a_failed_fetch_counts() {
        let dir = tempfile::tempdir().unwrap();
        let (cfg, rt) = node(dir.path());
        let w = Wakeups::new(|_, _, _| Ok(()));
        let mut failing = HashMap::new();
        // Nothing listens on port 1, so the fetch fails.
        let cmd = fork_from_elsewhere("sb", "127.0.0.1:1");

        match run_one(&cfg, &rt, &mut failing, &w, &cmd, Instant::now()) {
            Step::Later(d) => assert_eq!(d, FETCH_RECHECK),
            _ => panic!("a fork whose image is not here yet must wait for it"),
        }
        assert!(failing.is_empty(), "waiting on a fetch is not a failure");

        for _ in 0..200 {
            run_one(&cfg, &rt, &mut failing, &w, &cmd, Instant::now());
            if let Some(f) = failing.get("sb") {
                assert!(f.error.contains("fetching tag t from src"), "{}", f.error);
                // Nothing was set up for a sandbox that never started.
                assert!(!rt.tags_dir().join("t").exists());
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the failed fetch never reached the failure book");
    }

    #[test]
    fn a_dropped_tag_leaves_the_disk() {
        let dir = tempfile::tempdir().unwrap();
        let (cfg, rt) = node(dir.path());
        let w = Wakeups::new(|_, _, _| Ok(()));
        let mut failing = HashMap::new();
        std::fs::create_dir_all(rt.tags_dir().join("old")).unwrap();
        std::fs::write(rt.tags_dir().join("old/pages.img"), b"x").unwrap();

        let cmd = Command::DropTag { tag: "old".into() };
        assert!(matches!(
            run_one(&cfg, &rt, &mut failing, &w, &cmd, Instant::now()),
            Step::Done
        ));
        assert!(!rt.tags_dir().join("old").exists());
        assert!(failing.is_empty());
    }
}
