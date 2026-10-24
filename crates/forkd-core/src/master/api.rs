//! The master's HTTP surface: sandboxes for callers, poll and register for
//! nodes.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{oneshot, Notify};

use crate::auth::{require_token, AuthConfig};
use crate::identity::{random_token, Ca};
use crate::proto::{
    Command, ExecResult, PollRequest, PollResponse, RegisterRequest, RegisterResponse,
};
use crate::state::{now_secs, Node, Sandbox, Store};
use crate::tier::{IdlePolicy, Tier};

pub struct MasterState {
    pub store: Mutex<Store>,
    pub ca: Mutex<Ca>,
    pub idle: IdlePolicy,
    /// Range the master allocates sandbox ports from, per node.
    pub port_range: (u16, u16),
    /// Highest network slot a node will hand out, which caps sandboxes per
    /// node at the size of its address plan.
    pub max_net_index: u32,
    /// Refuse to place a sandbox on a node below this much free memory, or
    /// above this memory-pressure average.
    ///
    /// Measured on bm (2 cores, 3.6 GB, 2 GB swap) while filling the node
    /// 20 sandboxes at a time:
    ///
    ///   60 sandboxes   1014 MiB free   PSI avg10  2.5   healthy
    ///   80             673             PSI  4.9         swap engaging
    ///  100             579             PSI 11.8         degrading
    ///  120             408             PSI 15.6         degrading fast
    ///  139             391             PSI 61.8         node stopped
    ///                                                   answering SSH
    ///
    /// The master had no view of any of this: the node reports free memory
    /// and pressure on every poll and the master discarded both, so it kept
    /// accepting creates until the machine was unusable. "It fits" and "it
    /// still works" are different numbers, and admission is what keeps them
    /// apart.
    pub admit: Admission,
    /// One waker per node, so a change to desired state reaches the node's
    /// held-open connection at once instead of waiting for its next poll.
    wake: Mutex<HashMap<String, Arc<Notify>>>,
    /// Exec commands waiting to be handed to a node.
    ///
    /// Tier changes are derived from desired state on every poll, so they
    /// need no queue. Exec runs once and someone is waiting for its output,
    /// so it has to be delivered rather than recomputed.
    exec_queue: Mutex<HashMap<String, VecDeque<Command>>>,
    /// Callers parked on an exec, keyed by exec id.
    pending_exec: Mutex<HashMap<String, oneshot::Sender<ExecResult>>>,
    /// Bearer token the API requires. `None` leaves it open, which is only
    /// for tests: measured on bm, code inside a sandbox can reach the
    /// master over loopback, so an unauthenticated control plane means a
    /// sandbox can mint node tokens and delete other sandboxes.
    api_token: Option<Arc<String>>,
}

/// When a node is too loaded to take another sandbox.
#[derive(Debug, Clone, Copy)]
pub struct Admission {
    pub min_free_mib: u64,
    pub max_psi_avg10: f64,
}

impl Default for Admission {
    fn default() -> Self {
        // Below the point where bm began degrading, not at it: PSI is a
        // trailing average, so by the time it reads 10 the node has already
        // been under pressure for a while.
        Self {
            min_free_mib: 512,
            max_psi_avg10: 8.0,
        }
    }
}

impl Admission {
    /// Why this node cannot take another sandbox, if it cannot.
    pub fn refuse(&self, n: &Node) -> Option<String> {
        // A node that has not reported yet reads as 0 free, which must not
        // be mistaken for an exhausted node.
        if n.free_mib > 0 && n.free_mib < self.min_free_mib {
            return Some(format!(
                "{} MiB free, below the {} MiB floor",
                n.free_mib, self.min_free_mib
            ));
        }
        if n.psi_some_avg10 > self.max_psi_avg10 {
            return Some(format!(
                "memory pressure {:.1} above the {:.1} ceiling",
                n.psi_some_avg10, self.max_psi_avg10
            ));
        }
        None
    }
}

/// Everything the master is configured with, so adding one more knob does
/// not mean another positional argument nobody can read at the call site.
#[derive(Debug, Clone)]
pub struct MasterOptions {
    pub idle: IdlePolicy,
    pub port_range: (u16, u16),
    pub admit: Admission,
    pub api_token: Option<String>,
    pub max_net_index: u32,
}

impl Default for MasterOptions {
    fn default() -> Self {
        Self {
            idle: IdlePolicy::default(),
            port_range: (20000, 32000),
            admit: Admission::default(),
            api_token: None,
            // A /16 in /30s.
            max_net_index: 16383,
        }
    }
}

impl MasterState {
    pub fn new(store: Store, ca: Ca, opts: MasterOptions) -> Self {
        Self {
            store: Mutex::new(store),
            ca: Mutex::new(ca),
            idle: opts.idle,
            port_range: opts.port_range,
            admit: opts.admit,
            wake: Mutex::new(HashMap::new()),
            exec_queue: Mutex::new(HashMap::new()),
            pending_exec: Mutex::new(HashMap::new()),
            api_token: opts.api_token.map(Arc::new),
            max_net_index: opts.max_net_index,
        }
    }

    /// The token a freshly registered node needs for its later calls.
    fn api_token(&self) -> String {
        self.api_token.as_deref().cloned().unwrap_or_default()
    }

    fn waker(&self, node_id: &str) -> Arc<Notify> {
        self.wake
            .lock()
            .entry(node_id.to_string())
            .or_default()
            .clone()
    }

    /// Tell a node something it holds has changed. `notify_one` leaves a
    /// permit behind, so a change that lands between the node computing its
    /// work and settling down to wait is not lost -- it just wakes
    /// immediately on the next wait.
    ///
    /// The cost is one spurious early return when a wake arrives while no
    /// request is parked: the next park consumes the stale permit, finds
    /// nothing to do and reopens. It is self-correcting, since that second
    /// park has no permit, and losing a real change would be far worse.
    pub fn wake_node(&self, node_id: &str) {
        self.waker(node_id).notify_one();
    }
}

type St = Arc<MasterState>;

/// After this many consecutive failures the master stops issuing commands
/// for a sandbox. Retrying a hopeless command for ever is not resilience:
/// it is a permanent CPU cost on the node and an error nobody looks at.
/// The sandbox shows as failed, and `POST /v1/sandboxes/:id/retry` resumes
/// it once whatever was wrong has been fixed.
pub const MAX_FAILURES: u32 = 5;

/// Port a sandbox listens on inside its own namespace. Fixed rather than
/// allocated: each sandbox has its own network namespace, so two may both
/// use it, and a node-wide port space no longer exists.
pub const DEFAULT_SANDBOX_PORT: u16 = 8080;

pub fn router(st: St) -> Router {
    // Open: a health probe needs no credential, and registration carries
    // its own single-use token -- a node has nothing else yet.
    let open = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/v1/node/register", post(register))
        .with_state(st.clone());

    let cfg = match st.api_token.as_ref() {
        Some(t) => AuthConfig::with_token(t.as_str()),
        None => AuthConfig::open(),
    };
    let guarded = Router::new()
        .route("/v1/nodes", get(list_nodes))
        .route("/v1/node-tokens", post(create_node_token))
        .route("/v1/node-bootstrap", post(node_bootstrap))
        .route("/v1/node/poll", post(poll))
        .route("/v1/node/exec-result", post(exec_result))
        .route("/v1/sandboxes", get(list_sandboxes).post(create_sandbox))
        .route("/v1/sandboxes/:id", get(get_sandbox).delete(delete_sandbox))
        .route("/v1/sandboxes/:id/touch", post(touch_sandbox))
        .route("/v1/sandboxes/:id/retry", post(retry_sandbox))
        .route("/v1/sandboxes/:id/tier", post(set_tier))
        .route("/v1/sandboxes/:id/exec", post(exec_sandbox))
        .route("/v1/templates", get(list_templates).post(put_template))
        .layer(axum::middleware::from_fn(move |req, next| {
            let cfg = cfg.clone();
            async move { require_token(cfg, req, next).await }
        }))
        .with_state(st);

    open.merge(guarded)
}

fn err(code: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<serde_json::Value>) {
    (code, Json(serde_json::json!({ "error": msg.into() })))
}
type ApiErr = (StatusCode, Json<serde_json::Value>);

// ---- nodes --------------------------------------------------------------

#[derive(Serialize)]
struct NodeView {
    id: String,
    pool: String,
    fingerprint: String,
    max_sandboxes: u32,
    connected: bool,
    last_seen_at: i64,
    free_mib: u64,
    psi_some_avg10: f64,
    /// Present when this node would currently refuse a new sandbox.
    #[serde(skip_serializing_if = "Option::is_none")]
    not_admitting: Option<String>,
}

async fn list_nodes(State(st): State<St>) -> Result<Json<Vec<NodeView>>, ApiErr> {
    let nodes = st
        .store
        .lock()
        .list_nodes()
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(
        nodes
            .into_iter()
            .map(|n| NodeView {
                not_admitting: st.admit.refuse(&n),
                id: n.id,
                pool: n.pool,
                fingerprint: n.fingerprint,
                max_sandboxes: n.max_sandboxes,
                connected: n.connected,
                last_seen_at: n.last_seen_at,
                free_mib: n.free_mib,
                psi_some_avg10: n.psi_some_avg10,
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
struct NewNodeToken {
    #[serde(default = "default_pool")]
    pool: String,
    /// Short on purpose. A registration token is a bootstrap credential;
    /// it should not outlive the operator's terminal session.
    #[serde(default = "default_ttl")]
    ttl_secs: i64,
}
fn default_pool() -> String {
    "default".into()
}
fn default_ttl() -> i64 {
    900
}

#[derive(Serialize)]
struct NodeTokenView {
    token: String,
    pool: String,
    expires_at: i64,
}

async fn create_node_token(
    State(st): State<St>,
    Json(req): Json<NewNodeToken>,
) -> Result<Json<NodeTokenView>, ApiErr> {
    let token = random_token();
    st.store
        .lock()
        .create_node_token(&token, &req.pool, req.ttl_secs)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(NodeTokenView {
        token,
        pool: req.pool,
        expires_at: now_secs() + req.ttl_secs,
    }))
}

#[derive(Deserialize)]
struct BootstrapReq {
    #[serde(default = "default_pool")]
    pool: String,
    #[serde(default = "default_ttl")]
    ttl_secs: i64,
    /// How the new machine should reach this master. The master cannot work
    /// this out for itself: the address a node must dial is whatever the
    /// operator's network puts in front of us, not the socket we bound.
    master_url: String,
}

#[derive(Serialize)]
struct BootstrapView {
    token: String,
    pool: String,
    expires_at: i64,
    /// What the panel shows for "add a machine". It is a command, not a
    /// machine: the master cannot reach a host that has never dialled in,
    /// so one line has to be run over there. Same shape as k3s, Tailscale
    /// or an Actions runner, and for the same reason.
    install_command: String,
}

/// Mint a token and render the one line that joins a new machine.
async fn node_bootstrap(
    State(st): State<St>,
    Json(req): Json<BootstrapReq>,
) -> Result<Json<BootstrapView>, ApiErr> {
    let token = random_token();
    st.store
        .lock()
        .create_node_token(&token, &req.pool, req.ttl_secs)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let master = req.master_url.trim_end_matches('/');
    Ok(Json(BootstrapView {
        install_command: format!(
            "curl -sSfL {master}/install.sh | sudo FORKD_MASTER_URL={master} \\\n    FORKD_NODE_TOKEN={token} sh"
        ),
        token,
        pool: req.pool,
        expires_at: now_secs() + req.ttl_secs,
    }))
}

/// Trade a single-use registration token for a client certificate.
/// This is the one endpoint a node reaches without a certificate, so it is
/// also the only place the token path can be abused -- hence single use,
/// short TTL, and the node id fixed into the certificate's CN.
async fn register(
    State(st): State<St>,
    Json(req): Json<RegisterRequest>,
) -> Result<Json<RegisterResponse>, ApiErr> {
    let pool = {
        let store = st.store.lock();
        store
            .redeem_node_token(&req.token, &req.node_id)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    };
    let Some(pool) = pool else {
        return Err(err(
            StatusCode::UNAUTHORIZED,
            "registration token is unknown, expired, or already used",
        ));
    };

    let (cert, key, ca_pem) = {
        let ca = st.ca.lock();
        let paths = ca
            .issue_node(&req.node_id)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let cert = std::fs::read_to_string(&paths.cert)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let key = std::fs::read_to_string(&paths.key)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        (cert, key, ca.cert_pem())
    };

    st.store
        .lock()
        .upsert_node(&Node {
            id: req.node_id.clone(),
            pool: pool.clone(),
            fingerprint: req.fingerprint,
            max_sandboxes: req.max_sandboxes,
            last_seen_at: now_secs(),
            connected: true,
            free_mib: 0,
            psi_some_avg10: 0.0,
        })
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    tracing::info!(node = %req.node_id, pool = %pool, "node registered");
    Ok(Json(RegisterResponse {
        node_id: req.node_id,
        pool,
        client_cert: cert,
        client_key: key,
        ca_cert: ca_pem,
        // The node's later calls are on the guarded routes, so it is handed
        // the token here -- the one moment it is already authenticated, by
        // the single-use registration token it just spent.
        api_token: st.api_token(),
    }))
}

/// A node reports what it holds and collects work. Desired state is compared
/// against the node's observation, never against a cached view.
async fn poll(
    State(st): State<St>,
    Json(req): Json<PollRequest>,
) -> Result<Json<PollResponse>, ApiErr> {
    // Held open, not polled. The node dials in and this request stays
    // parked until there is work for it, so dispatch costs one round trip
    // rather than up to a poll interval -- the operator-visible behaviour
    // of a machine that accepts scheduling the moment its service is up,
    // without the node listening on anything.
    record_failures(&st, &req)?;

    let mut commands = diff(&st, &req)?;
    commands.extend(take_execs(&st, &req.node_id));
    if !commands.is_empty() {
        return Ok(Json(PollResponse { commands }));
    }

    let waker = st.waker(&req.node_id);
    let hold = Duration::from_millis(req.hold_ms.unwrap_or(25_000).min(55_000));
    if !hold.is_zero() {
        // A timeout as well as a wake, so the connection is refreshed
        // periodically and a node that has silently gone away is noticed.
        tokio::time::timeout(hold, waker.notified()).await.ok();
    }
    let mut commands = diff(&st, &req)?;
    commands.extend(take_execs(&st, &req.node_id));
    Ok(Json(PollResponse { commands }))
}

fn take_execs(st: &St, node_id: &str) -> Vec<Command> {
    let mut q = st.exec_queue.lock();
    match q.get_mut(node_id) {
        Some(d) => d.drain(..).collect(),
        None => Vec::new(),
    }
}

#[derive(Deserialize)]
struct SetTierReq {
    tier: Tier,
}

/// Ask for a tier directly, rather than waiting for the idle policy.
///
/// Demotion still goes one step at a time -- the node has no direct
/// hot -> detached transition, because each leg is cheap and a jump would
/// stall it on a tar. So this walks the intermediate tiers itself.
async fn set_tier(
    State(st): State<St>,
    Path(id): Path<String>,
    Json(req): Json<SetTierReq>,
) -> Result<Json<SandboxView>, ApiErr> {
    let (node_id, view) = {
        let store = st.store.lock();
        let Some(sb) = store
            .get_sandbox(&id)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        else {
            return Err(err(StatusCode::NOT_FOUND, "no such sandbox"));
        };
        store
            .set_tier(&id, req.tier)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        if req.tier == Tier::Hot {
            store
                .touch(&id)
                .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        }
        let fresh = store
            .get_sandbox(&id)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            .ok_or_else(|| err(StatusCode::NOT_FOUND, "no such sandbox"))?;
        (sb.node_id, fresh)
    };
    st.wake_node(&node_id);
    Ok(Json(view.into()))
}

/// Clear a sandbox's failure so the node is asked to try again.
async fn retry_sandbox(
    State(st): State<St>,
    Path(id): Path<String>,
) -> Result<Json<SandboxView>, ApiErr> {
    let (node_id, view) = {
        let store = st.store.lock();
        let Some(sb) = store
            .get_sandbox(&id)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        else {
            return Err(err(StatusCode::NOT_FOUND, "no such sandbox"));
        };
        store
            .clear_sandbox_error(&id)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let fresh = store
            .get_sandbox(&id)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            .ok_or_else(|| err(StatusCode::NOT_FOUND, "no such sandbox"))?;
        (sb.node_id, fresh)
    };
    st.wake_node(&node_id);
    Ok(Json(view.into()))
}

#[derive(Deserialize)]
struct ExecReq {
    args: Vec<String>,
    #[serde(default = "default_exec_timeout")]
    timeout_secs: u64,
}
fn default_exec_timeout() -> u64 {
    30
}

#[derive(Serialize)]
struct ExecView {
    stdout: String,
    stderr: String,
    exit_code: i64,
}

/// Run a command in a sandbox and wait for its output.
///
/// Same request and response shape as the v0.5 controller, so the SDKs do
/// not have to change for this call.
async fn exec_sandbox(
    State(st): State<St>,
    Path(id): Path<String>,
    Json(req): Json<ExecReq>,
) -> Result<Json<ExecView>, ApiErr> {
    if req.args.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "args must not be empty"));
    }

    let (node_id, tier) = {
        let store = st.store.lock();
        let Some(sb) = store
            .get_sandbox(&id)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        else {
            return Err(err(StatusCode::NOT_FOUND, "no such sandbox"));
        };
        (sb.node_id, sb.tier)
    };

    // A command can only run in a sandbox that is running. Promoting first
    // would hide a multi-second wait inside what looks like a quick call,
    // so say so instead and let the caller decide.
    if tier != Tier::Hot {
        return Err(err(
            StatusCode::CONFLICT,
            format!("sandbox is {tier}; POST /v1/sandboxes/{id}/touch to bring it hot first"),
        ));
    }

    let exec_id = random_token();
    let (tx, rx) = oneshot::channel();
    {
        st.pending_exec.lock().insert(exec_id.clone(), tx);
        st.exec_queue
            .lock()
            .entry(node_id.clone())
            .or_default()
            .push_back(Command::Exec {
                exec_id: exec_id.clone(),
                id: id.clone(),
                args: req.args,
                timeout_secs: req.timeout_secs,
            });
    }
    st.wake_node(&node_id);

    // The node's own timeout plus room for a round trip and a loaded node.
    let budget = Duration::from_secs(req.timeout_secs.saturating_add(20));
    match tokio::time::timeout(budget, rx).await {
        Ok(Ok(r)) => Ok(Json(ExecView {
            stdout: r.stdout,
            stderr: r.stderr,
            exit_code: r.exit_code,
        })),
        // Sender dropped, or nothing came back in time. Either way the
        // caller gets an answer rather than hanging.
        _ => {
            st.pending_exec.lock().remove(&exec_id);
            Err(err(
                StatusCode::GATEWAY_TIMEOUT,
                format!(
                    "node {node_id} did not return a result within {}s",
                    budget.as_secs()
                ),
            ))
        }
    }
}

/// The node reporting one exec's output.
async fn exec_result(
    State(st): State<St>,
    Json(r): Json<ExecResult>,
) -> Result<StatusCode, ApiErr> {
    let tx = st.pending_exec.lock().remove(&r.exec_id);
    match tx {
        Some(tx) => {
            // The caller may already have timed out and gone.
            let _ = tx.send(r);
            Ok(StatusCode::NO_CONTENT)
        }
        None => Err(err(
            StatusCode::NOT_FOUND,
            "no caller is waiting on that exec",
        )),
    }
}

/// Keep the reason a sandbox is not coming up where an operator can see it.
fn record_failures(st: &St, req: &PollRequest) -> Result<(), ApiErr> {
    if req.failures.is_empty() {
        return Ok(());
    }
    let store = st.store.lock();
    for f in &req.failures {
        store
            .set_sandbox_error(&f.id, &f.error, f.attempts)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        if f.attempts >= MAX_FAILURES {
            tracing::error!(
                sandbox = %f.id, node = %req.node_id, attempts = f.attempts,
                error = %f.error,
                "sandbox quarantined after repeated failures; POST /retry to resume"
            );
        } else {
            tracing::warn!(
                sandbox = %f.id, node = %req.node_id, attempts = f.attempts,
                error = %f.error, "node reported a failing sandbox"
            );
        }
    }
    Ok(())
}

/// Desired state minus what the node reports holding. Sync and short, so the
/// store lock is never held across an await.
fn diff(st: &St, req: &PollRequest) -> Result<Vec<Command>, ApiErr> {
    let store = st.store.lock();
    store
        .set_node_health(&req.node_id, req.free_memory_mib, req.pressure_some_avg10)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // The node owns addressing, so what it reports is the truth.
    for o in &req.observed {
        if let Some(ip) = o.ip.as_deref() {
            store
                .set_sandbox_ip(&o.id, ip)
                .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        }
    }

    let desired = store
        .sandboxes_on_node(&req.node_id)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let mut commands = Vec::new();
    for want in &desired {
        // Quarantined: keep it in desired state and visible, but stop asking
        // the node to do something that has failed MAX_FAILURES times.
        if want.error_attempts >= MAX_FAILURES {
            continue;
        }
        match req.observed.iter().find(|o| o.id == want.id) {
            // Detached means nothing on any node, so a node not holding it
            // IS the desired state. Without this the tier resurrects itself
            // the instant it is reached, and the whole point of detached --
            // that it costs nothing -- is lost.
            None if want.tier == Tier::Detached => {}
            None => {
                let spec = match store
                    .get_template(&want.template)
                    .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
                {
                    Some((_rootfs, spec)) => {
                        serde_json::from_str(&spec).unwrap_or(serde_json::Value::Null)
                    }
                    None => {
                        // The template was removed after the sandbox was
                        // created. Say so rather than sending a Create the
                        // node can only fail on.
                        tracing::warn!(
                            sandbox = %want.id, template = %want.template,
                            "template is gone; not issuing Create"
                        );
                        continue;
                    }
                };
                commands.push(Command::Create {
                    id: want.id.clone(),
                    template: want.template.clone(),
                    spec,
                    port: want.port.unwrap_or(DEFAULT_SANDBOX_PORT),
                    volume: want.volume.clone(),
                    net_index: want.net_index,
                })
            }
            Some(got) if got.tier != want.tier => commands.push(Command::SetTier {
                id: want.id.clone(),
                tier: want.tier,
            }),
            Some(_) => {}
        }
    }
    // Anything the node holds that we no longer want. Adoption means we only
    // ever destroy what desired state has actually dropped.
    for got in &req.observed {
        if !desired.iter().any(|d| d.id == got.id) {
            // The node also needs to know which slot to reclaim; look it
            // up, since an observed-but-undesired sandbox has no row to
            // read it from once desired state has dropped it.
            commands.push(Command::Destroy {
                id: got.id.clone(),
                net_index: got.net_index,
            });
        }
    }
    Ok(commands)
}

// ---- sandboxes ----------------------------------------------------------

#[derive(Serialize)]
struct SandboxView {
    id: String,
    node_id: String,
    pool: String,
    template: String,
    tier: Tier,
    volume: Option<String>,
    port: Option<u16>,
    /// Where to reach it. Set once the node reports the address it gave the
    /// sandbox; absent until then.
    #[serde(skip_serializing_if = "Option::is_none")]
    ip: Option<String>,
    created_at: i64,
    last_active_at: i64,
    /// Why it is not up, when it is not. The panel shows this instead of
    /// leaving an operator to guess from a node's log.
    #[serde(skip_serializing_if = "Option::is_none")]
    last_error: Option<String>,
    #[serde(skip_serializing_if = "is_zero")]
    error_attempts: u32,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

impl From<Sandbox> for SandboxView {
    fn from(s: Sandbox) -> Self {
        // Note the absence of `token`: it is returned once, at creation.
        Self {
            id: s.id,
            node_id: s.node_id,
            pool: s.pool,
            template: s.template,
            tier: s.tier,
            volume: s.volume,
            port: s.port,
            ip: s.ip,
            created_at: s.created_at,
            last_active_at: s.last_active_at,
            last_error: s.last_error,
            error_attempts: s.error_attempts,
        }
    }
}

#[derive(Deserialize)]
struct NewSandbox {
    #[serde(default)]
    id: Option<String>,
    template: String,
    #[serde(default)]
    pool: Option<String>,
    #[serde(default)]
    node_id: Option<String>,
    #[serde(default)]
    volume: Option<String>,
    #[serde(default)]
    port: Option<u16>,
}

#[derive(Serialize)]
struct NewSandboxView {
    #[serde(flatten)]
    sandbox: SandboxView,
    /// Shown exactly once. The master keeps it to compare against, but the
    /// API never hands it out again.
    token: String,
}

async fn create_sandbox(
    State(st): State<St>,
    Json(req): Json<NewSandbox>,
) -> Result<(StatusCode, Json<NewSandboxView>), ApiErr> {
    let store = st.store.lock();

    if store
        .get_template(&req.template)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .is_none()
    {
        return Err(err(
            StatusCode::BAD_REQUEST,
            format!("unknown template: {}", req.template),
        ));
    }

    let pool = req.pool.unwrap_or_else(|| "default".into());
    let node_id = match req.node_id {
        Some(n) => n,
        None => {
            // Cheapest placement that respects pools: least-loaded connected
            // node in the pool. Snapshots never cross a pool boundary, so
            // this choice also fixes where the sandbox can ever be restored.
            let nodes = store
                .list_nodes()
                .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            let mut best: Option<(String, usize)> = None;
            let mut refused: Vec<String> = Vec::new();
            for n in nodes.into_iter().filter(|n| n.connected && n.pool == pool) {
                if let Some(why) = st.admit.refuse(&n) {
                    refused.push(format!("{}: {why}", n.id));
                    continue;
                }
                let load = store
                    .sandboxes_on_node(&n.id)
                    .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
                    .len();
                if best.as_ref().is_none_or(|(_, b)| load < *b) {
                    best = Some((n.id, load));
                }
            }
            match best {
                Some((id, _)) => id,
                None if !refused.is_empty() => {
                    // Say which nodes declined and why, so the limit reads as
                    // a decision rather than a mystery.
                    return Err(err(
                        StatusCode::SERVICE_UNAVAILABLE,
                        format!(
                            "every node in pool {pool} is too loaded: {}",
                            refused.join("; ")
                        ),
                    ));
                }
                None => {
                    return Err(err(
                        StatusCode::SERVICE_UNAVAILABLE,
                        format!("no connected node in pool {pool}"),
                    ))
                }
            }
        }
    };

    // The port is the sandbox's own, inside its namespace, so there is
    // nothing to allocate -- two sandboxes may both listen on 8080.
    let port = Some(req.port.unwrap_or(DEFAULT_SANDBOX_PORT));

    // The address plan is the node-local resource now.
    let net_index = match store
        .allocate_net_index(&node_id, st.max_net_index)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    {
        Some(i) => Some(i),
        None => {
            return Err(err(
                StatusCode::SERVICE_UNAVAILABLE,
                format!("node {node_id} has no free network slot"),
            ))
        }
    };

    let t = now_secs();
    let sb = Sandbox {
        id: req
            .id
            .unwrap_or_else(|| format!("sb-{}", &random_token()[..12])),
        node_id,
        pool,
        template: req.template,
        tier: Tier::Hot,
        token: random_token(),
        volume: req.volume,
        port,
        net_index,
        created_at: t,
        last_active_at: t,
        last_error: None,
        error_attempts: 0,
        ip: None,
    };
    store
        .insert_sandbox(&sb)
        .map_err(|e| err(StatusCode::CONFLICT, e.to_string()))?;

    // The node is parked on its stream; this reaches it now.
    st.wake_node(&sb.node_id);

    let token = sb.token.clone();
    Ok((
        StatusCode::CREATED,
        Json(NewSandboxView {
            sandbox: sb.into(),
            token,
        }),
    ))
}

async fn list_sandboxes(State(st): State<St>) -> Result<Json<Vec<SandboxView>>, ApiErr> {
    let all = st
        .store
        .lock()
        .list_sandboxes()
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(all.into_iter().map(Into::into).collect()))
}

async fn get_sandbox(
    State(st): State<St>,
    Path(id): Path<String>,
) -> Result<Json<SandboxView>, ApiErr> {
    let got = st
        .store
        .lock()
        .get_sandbox(&id)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    got.map(|s| Json(s.into()))
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "no such sandbox"))
}

/// Mark a sandbox used. Promotion to hot is what a real request does; this
/// just resets the idle clock so the demoter leaves it alone.
async fn touch_sandbox(
    State(st): State<St>,
    Path(id): Path<String>,
) -> Result<Json<SandboxView>, ApiErr> {
    let store = st.store.lock();
    let Some(sb) = store
        .get_sandbox(&id)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    else {
        return Err(err(StatusCode::NOT_FOUND, "no such sandbox"));
    };
    store
        .touch(&id)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if sb.tier != Tier::Hot {
        store
            .set_tier(&id, Tier::Hot)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        // Promotion is the latency-critical path: a caller is waiting.
        st.wake_node(&sb.node_id);
    }
    let fresh = store
        .get_sandbox(&id)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "no such sandbox"))?;
    Ok(Json(fresh.into()))
}

async fn delete_sandbox(
    State(st): State<St>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiErr> {
    // Read the node before dropping the row: afterwards there is nothing
    // left to say which node to wake.
    let node_id = {
        let store = st.store.lock();
        let node_id = store
            .get_sandbox(&id)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            .map(|s| s.node_id);
        store
            .delete_sandbox(&id)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        node_id
    };
    // The node tears it down when it sees it holds a sandbox desired state
    // no longer lists -- but only once it polls, so wake it. Without this
    // the teardown waits out the node's whole hold: measured on bm, a
    // deleted sandbox kept running for 30 s with a 25 s hold.
    if let Some(n) = node_id {
        st.wake_node(&n);
    }
    Ok(StatusCode::NO_CONTENT)
}

// ---- templates ----------------------------------------------------------

#[derive(Deserialize)]
struct NewTemplate {
    name: String,
    rootfs: String,
    /// OCI spec as JSON. Kept opaque here so a template can carry mounts and
    /// limits without this type having to know about them.
    spec: serde_json::Value,
}

async fn put_template(
    State(st): State<St>,
    Json(req): Json<NewTemplate>,
) -> Result<StatusCode, ApiErr> {
    st.store
        .lock()
        .put_template(&req.name, &req.rootfs, &req.spec.to_string())
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(StatusCode::CREATED)
}

async fn list_templates(State(st): State<St>) -> Result<Json<Vec<String>>, ApiErr> {
    let names = st
        .store
        .lock()
        .list_templates()
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(names))
}
