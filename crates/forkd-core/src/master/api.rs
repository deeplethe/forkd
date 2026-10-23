//! The master's HTTP surface: sandboxes for callers, poll and register for
//! nodes.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

use crate::identity::{random_token, Ca};
use crate::proto::{Command, PollRequest, PollResponse, RegisterRequest, RegisterResponse};
use crate::state::{now_secs, Node, Sandbox, Store};
use crate::tier::{IdlePolicy, Tier};

pub struct MasterState {
    pub store: Mutex<Store>,
    pub ca: Mutex<Ca>,
    pub idle: IdlePolicy,
    /// One waker per node, so a change to desired state reaches the node's
    /// held-open connection at once instead of waiting for its next poll.
    wake: Mutex<HashMap<String, Arc<Notify>>>,
}

impl MasterState {
    pub fn new(store: Store, ca: Ca, idle: IdlePolicy) -> Self {
        Self {
            store: Mutex::new(store),
            ca: Mutex::new(ca),
            idle,
            wake: Mutex::new(HashMap::new()),
        }
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

pub fn router(st: St) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/v1/nodes", get(list_nodes))
        .route("/v1/node-tokens", post(create_node_token))
        .route("/v1/node-bootstrap", post(node_bootstrap))
        .route("/v1/node/register", post(register))
        .route("/v1/node/poll", post(poll))
        .route("/v1/sandboxes", get(list_sandboxes).post(create_sandbox))
        .route("/v1/sandboxes/:id", get(get_sandbox).delete(delete_sandbox))
        .route("/v1/sandboxes/:id/touch", post(touch_sandbox))
        .route("/v1/sandboxes/:id/retry", post(retry_sandbox))
        .route("/v1/sandboxes/:id/tier", post(set_tier))
        .route("/v1/templates", get(list_templates).post(put_template))
        .with_state(st)
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
                id: n.id,
                pool: n.pool,
                fingerprint: n.fingerprint,
                max_sandboxes: n.max_sandboxes,
                connected: n.connected,
                last_seen_at: n.last_seen_at,
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
        })
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    tracing::info!(node = %req.node_id, pool = %pool, "node registered");
    Ok(Json(RegisterResponse {
        node_id: req.node_id,
        pool,
        client_cert: cert,
        client_key: key,
        ca_cert: ca_pem,
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

    let commands = diff(&st, &req)?;
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
    Ok(Json(PollResponse {
        commands: diff(&st, &req)?,
    }))
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
        .set_node_connected(&req.node_id, true)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

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
                    port: want.port.unwrap_or(0),
                    volume: want.volume.clone(),
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
            commands.push(Command::Destroy { id: got.id.clone() });
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
            for n in nodes.into_iter().filter(|n| n.connected && n.pool == pool) {
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
                None => {
                    return Err(err(
                        StatusCode::SERVICE_UNAVAILABLE,
                        format!("no connected node in pool {pool}"),
                    ))
                }
            }
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
        port: req.port,
        created_at: t,
        last_active_at: t,
        last_error: None,
        error_attempts: 0,
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
    st.store
        .lock()
        .delete_sandbox(&id)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    // The node tears it down on its next poll, when it sees it holds a
    // sandbox desired state no longer lists.
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
