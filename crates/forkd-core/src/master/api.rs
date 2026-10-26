//! The master's HTTP surface: sandboxes for callers, poll and register for
//! nodes.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{oneshot, Notify};

use super::metrics;
use crate::auth::{require_token, AuthConfig};
use crate::identity::{random_token, Ca};
use crate::proto::{
    Command, CommandResult, PollRequest, PollResponse, RegisterRequest, RegisterResponse,
};
use crate::state::{now_secs, Node, Sandbox, SnapshotTag, Store};
use crate::tier::{IdlePolicy, Tier};

pub struct MasterState {
    /// Private on purpose: `store()` is the only way in, so that no call
    /// site can take the mutex without being timed. See `StoreGuard`.
    store: Mutex<Store>,
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
    /// Whether the node control channel demands a client certificate.
    ///
    /// True whenever the control plane serves TLS, which is the default.
    /// False only under `--insecure-transport`, where there is no
    /// certificate to demand -- see `NodeIdentity::Untrusted`.
    pub require_node_certs: bool,
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
    pending_exec: Mutex<HashMap<String, oneshot::Sender<CommandResult>>>,
    /// One task per node the master connects to. Kept so that re-adding a
    /// node replaces its driver instead of starting a second one, and so
    /// revoking a node stops the master talking to it.
    drivers: Mutex<HashMap<String, tokio::task::AbortHandle>>,
    /// What this master is doing, for `GET /metrics`.
    pub metrics: metrics::Metrics,
    /// The fleet's recent history, for the console. Set once by `serve`;
    /// empty in tests that build a state without it, where the console's
    /// history routes answer 503.
    pub series: std::sync::OnceLock<Arc<super::series::Series>>,
    /// Bearer token the API requires. `None` leaves it open, which is only
    /// for tests: measured on bm, code inside a sandbox can reach the
    /// master over loopback, so an unauthenticated control plane means a
    /// sandbox can mint node tokens and delete other sandboxes.
    api_token: Option<Arc<String>>,
}

/// A held store lock that reports how long it was held when it is dropped.
pub struct StoreGuard<'a> {
    inner: parking_lot::MutexGuard<'a, Store>,
    metrics: &'a metrics::Metrics,
    since: Instant,
}

impl std::ops::Deref for StoreGuard<'_> {
    type Target = Store;
    fn deref(&self) -> &Store {
        &self.inner
    }
}

impl Drop for StoreGuard<'_> {
    fn drop(&mut self) {
        self.metrics.store_held.observe(self.since.elapsed());
    }
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
    ///
    /// `load` is how many sandboxes it is already carrying. It is passed in
    /// rather than read here so that the scheduler and the panel answer
    /// from the same number: the scheduler needs the count anyway, and two
    /// separate reads could disagree across a concurrent create.
    pub fn refuse(&self, n: &Node, load: u32) -> Option<String> {
        // The cap is checked first because it is the one limit an operator
        // set deliberately; memory and pressure move on their own, so a
        // node that is both full and tight is more usefully reported full.
        //
        // `max_sandboxes` 0 means "as many as memory allows" -- that is what
        // `--max-sandboxes` documents, and it is the default.
        if n.max_sandboxes > 0 && load >= n.max_sandboxes {
            return Some(format!(
                "{load} sandboxes, at its cap of {}",
                n.max_sandboxes
            ));
        }
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
    /// See `MasterState::require_node_certs`.
    pub require_node_certs: bool,
}

impl Default for MasterOptions {
    fn default() -> Self {
        Self {
            idle: IdlePolicy::default(),
            port_range: (20000, 32000),
            admit: Admission::default(),
            api_token: None,
            // A /16 in /30s.
            // The top slot of a /16 is the node's own; see `NetConfig::reserved_index`.
            max_net_index: 16382,
            // Tests build the router without a TLS listener, so there is no
            // certificate to demand; the live default comes from
            // MasterConfig and is the opposite.
            require_node_certs: false,
        }
    }
}

impl MasterState {
    /// The CA fingerprint a node should pin, for embedding in a token.
    pub fn ca_pin(&self) -> Option<String> {
        crate::tls::fingerprint_of_pem(&self.ca.lock().cert_pem()).ok()
    }

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
            drivers: Mutex::new(HashMap::new()),
            api_token: opts.api_token.map(Arc::new),
            max_net_index: opts.max_net_index,
            require_node_certs: opts.require_node_certs,
            metrics: metrics::Metrics::default(),
            series: std::sync::OnceLock::new(),
        }
    }

    /// Take the store, timing both the wait for it and the hold.
    ///
    /// The field is private so this is the only way in. Every handler and
    /// the demotion sweep pass through here, which is what makes
    /// `forkd_store_lock_wait_seconds` a complete picture of control-plane
    /// contention rather than a sample of whoever remembered to measure.
    ///
    /// One mutex guards one sqlite connection, so this is where the master
    /// serialises; a load test that cannot see this number cannot tell a
    /// slow node from a queue in front of the database.
    pub fn store(&self) -> StoreGuard<'_> {
        let queued = Instant::now();
        let inner = self.store.lock();
        self.metrics.store_wait.observe(queued.elapsed());
        StoreGuard {
            inner,
            metrics: &self.metrics,
            since: Instant::now(),
        }
    }

    /// Record a node's driver, stopping any earlier one for the same node.
    pub(super) fn set_driver(&self, node_id: &str, handle: tokio::task::AbortHandle) {
        if let Some(old) = self.drivers.lock().insert(node_id.to_string(), handle) {
            old.abort();
        }
    }

    pub(super) fn stop_driver(&self, node_id: &str) {
        if let Some(h) = self.drivers.lock().remove(node_id) {
            h.abort();
        }
    }

    /// The token a freshly registered node needs for its later calls.
    fn api_token(&self) -> String {
        self.api_token.as_deref().cloned().unwrap_or_default()
    }

    pub(super) fn waker(&self, node_id: &str) -> Arc<Notify> {
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

pub(super) type St = Arc<MasterState>;

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

/// Most children one fork may make at once. A bound rather than a limit
/// anyone should hit: it stops a typo in `n` from filling a node's address
/// plan before admission control gets a word in.
pub const MAX_FORK_WIDTH: u32 = 256;

pub fn router(st: St) -> Router {
    // Open: a health probe needs no credential, and registration carries
    // its own single-use token -- a node has nothing else yet.
    // Open because a machine that has not joined yet holds no credential:
    // the registration token is what gates joining, and these three are
    // what the token's one-line command needs before it has one.
    let open = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/v1/node/register", post(register))
        .route("/", get(panel))
        // The page only: everything it shows comes from the guarded
        // routes below, with the token the operator types into it.
        .route("/console", get(console_page))
        .route("/install.sh", get(install_script))
        .route("/ca.pem", get(ca_pem))
        .route("/forkd-serve", get(own_binary))
        .with_state(st.clone());

    let cfg = match st.api_token.as_ref() {
        Some(t) => AuthConfig::with_token(t.as_str()),
        None => AuthConfig::open(),
    };
    // The node control channel. It carries the bearer token like everything
    // else, but a token alone is not enough here: it is one shared secret,
    // so any holder could poll as any node and be handed its sandboxes.
    // These two routes additionally require a client certificate, and the
    // node id is taken from it rather than from the body.
    let node_cfg = match st.api_token.as_ref() {
        Some(t) => AuthConfig::with_token(t.as_str()),
        None => AuthConfig::open(),
    };
    let node_routes = Router::new()
        .route("/v1/node/poll", post(poll))
        .route("/v1/node/traffic", post(node_traffic))
        .route("/v1/node/result", post(command_result))
        .layer(axum::middleware::from_fn({
            let st = st.clone();
            move |req, next| {
                let st = st.clone();
                async move { require_node_cert(st, req, next).await }
            }
        }))
        .layer(axum::middleware::from_fn(move |req, next| {
            let cfg = node_cfg.clone();
            async move { require_token(cfg, req, next).await }
        }))
        .with_state(st.clone());

    let guarded = Router::new()
        .route("/v1/nodes", get(list_nodes).post(add_node))
        .route("/v1/node-tokens", post(create_node_token))
        .route("/v1/node-bootstrap", post(node_bootstrap))
        .route("/v1/node/revoke/:id", post(revoke_node))
        .route("/v1/sandboxes", get(list_sandboxes).post(create_sandbox))
        .route("/v1/sandboxes/:id", get(get_sandbox).delete(delete_sandbox))
        .route("/v1/sandboxes/:id/touch", post(touch_sandbox))
        .route("/v1/sandboxes/:id/retry", post(retry_sandbox))
        .route("/v1/sandboxes/:id/tier", post(set_tier))
        .route("/v1/sandboxes/:id/exec", post(exec_sandbox))
        .route("/v1/sandboxes/:id/branch", post(branch_sandbox))
        .route("/v1/sandboxes/:id/eval", post(eval_sandbox))
        .route(
            "/v1/sandboxes/:id/ping",
            post(ping_sandbox).get(ping_sandbox),
        )
        .route("/v1/snapshots", get(list_snapshots))
        .route("/v1/snapshots/:tag", axum::routing::delete(delete_snapshot))
        .route("/v1/snapshots/:tag/info", get(snapshot_info))
        .route("/v1/templates", get(list_templates).post(put_template))
        // Behind the token like everything else: the exposition names every
        // node and its free memory, which is a map of the fleet.
        .route("/metrics", get(metrics_endpoint))
        .route("/v1/console/now", get(console_now))
        .route("/v1/console/series", get(console_series))
        .route("/v1/console/events", get(console_events))
        .layer(axum::middleware::from_fn(move |req, next| {
            let cfg = cfg.clone();
            async move { require_token(cfg, req, next).await }
        }))
        .with_state(st.clone());

    // Outermost, so it sees every request including the ones the auth layer
    // rejects -- a load test that is failing on 401s should be able to tell.
    open.merge(node_routes)
        .merge(guarded)
        .layer(axum::middleware::from_fn(move |req, next| {
            let st = st.clone();
            async move { track_request(st, req, next).await }
        }))
}

/// Time every request and file it under the route it matched.
///
/// `MatchedPath` rather than the URI: it is `/v1/sandboxes/:id`, so a
/// thousand sandbox ids stay one series instead of becoming a thousand.
async fn track_request(
    st: St,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let route = req
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map(|m| m.as_str().to_string())
        // A request that matched nothing is a 404. Bucketing them together
        // keeps a scanner from inventing a series per probed path.
        .unwrap_or_else(|| "<unmatched>".to_string());
    let method = req.method().as_str().to_string();
    let started = Instant::now();
    let res = next.run(req).await;
    st.metrics
        .observe_http(&method, &route, res.status().as_u16(), started.elapsed());
    res
}

async fn metrics_endpoint(State(st): State<St>) -> Result<impl IntoResponse, ApiErr> {
    let body = metrics::render_with(&st)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok((
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    ))
}

// ---- console ---------------------------------------------------------------

async fn console_page() -> impl IntoResponse {
    (
        [("content-type", "text/html; charset=utf-8")],
        include_str!("console.html"),
    )
}

#[derive(Deserialize)]
struct SeriesQuery {
    /// How far back, in seconds. Capped at what is kept.
    #[serde(default = "default_since")]
    since: i64,
    /// Seconds per point. Defaults to whatever gives about 240 points.
    #[serde(default)]
    step: Option<i64>,
}

fn default_since() -> i64 {
    900
}

fn console_history(st: &St) -> Result<Arc<super::series::Series>, ApiErr> {
    st.series.get().cloned().ok_or_else(|| {
        err(
            StatusCode::SERVICE_UNAVAILABLE,
            "this master is not recording history",
        )
    })
}

#[derive(Serialize)]
struct SeriesView {
    now: i64,
    since: i64,
    step: i64,
    every: u64,
    lines: Vec<super::series::Line>,
}

async fn console_series(
    State(st): State<St>,
    axum::extract::Query(q): axum::extract::Query<SeriesQuery>,
) -> Result<Json<SeriesView>, ApiErr> {
    let series = console_history(&st)?;
    let span = q.since.clamp(60, super::series::KEEP.as_secs() as i64);
    let every = super::series::EVERY.as_secs() as i64;
    let step = q.step.unwrap_or(span / 240).max(every);
    let now = now_secs();
    let since = now - span;
    let lines = tokio::task::spawn_blocking(move || series.lines(since, step))
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(SeriesView {
        now,
        since,
        step,
        every: every as u64,
        lines,
    }))
}

#[derive(Deserialize)]
struct EventsQuery {
    #[serde(default = "default_events_since")]
    since: i64,
    #[serde(default = "default_events_limit")]
    limit: u32,
}

fn default_events_since() -> i64 {
    3600
}

fn default_events_limit() -> u32 {
    100
}

async fn console_events(
    State(st): State<St>,
    axum::extract::Query(q): axum::extract::Query<EventsQuery>,
) -> Result<Json<Vec<super::series::Event>>, ApiErr> {
    let series = console_history(&st)?;
    let since = now_secs() - q.since.max(0);
    let limit = q.limit.clamp(1, 1000);
    let ev = tokio::task::spawn_blocking(move || series.events(since, limit))
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(ev))
}

#[derive(Serialize)]
struct ConsoleNode {
    id: String,
    pool: String,
    connected: bool,
    last_seen_age: i64,
    free_mib: u64,
    psi: f64,
    max_sandboxes: u32,
    tiers: HashMap<String, u32>,
    features: Vec<String>,
}

#[derive(Serialize)]
struct ConsoleFailing {
    id: String,
    node: String,
    error: String,
    attempts: u32,
}

#[derive(Serialize)]
struct ConsoleNow {
    now: i64,
    uptime_secs: u64,
    idle: [u64; 3],
    tiers: HashMap<String, u32>,
    failed: u32,
    snapshots: u32,
    snapshot_bytes: u64,
    templates: Vec<String>,
    nodes: Vec<ConsoleNode>,
    failing: Vec<ConsoleFailing>,
    counters: HashMap<&'static str, u64>,
}

/// Everything the console draws that is a single "now" rather than a line.
async fn console_now(State(st): State<St>) -> Result<Json<ConsoleNow>, ApiErr> {
    let ise = |e: anyhow::Error| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    let (nodes, by_tier, by_node_tier, failed, (snapshots, snapshot_bytes), failing, templates) = {
        let store = st.store();
        (
            store.list_nodes().map_err(ise)?,
            store.count_sandboxes_by_tier().map_err(ise)?,
            store.count_sandboxes_by_node_tier().map_err(ise)?,
            store.count_failed_sandboxes().map_err(ise)?,
            store.snapshot_totals().map_err(ise)?,
            store.failing_sandboxes(12).map_err(ise)?,
            store.list_templates().map_err(ise)?,
        )
    };
    let now = now_secs();
    let features = st.metrics.node_features.lock().clone();
    let mut tiers_by_node: HashMap<String, HashMap<String, u32>> = HashMap::new();
    for (node, tier, n) in by_node_tier {
        tiers_by_node.entry(node).or_default().insert(tier, n);
    }
    let m = &st.metrics;
    let demoted = m.demotions_total();
    let counters = HashMap::from([
        ("creates_placed", m.creates_placed.load(Relaxed)),
        ("creates_refused", m.creates_refused.load(Relaxed)),
        ("deletes", m.deletes.load(Relaxed)),
        ("woken", m.woken.load(Relaxed)),
        ("to_frozen", demoted[1]),
        ("to_snapshot", demoted[2]),
        ("to_detached", demoted[3]),
    ]);
    Ok(Json(ConsoleNow {
        now,
        uptime_secs: m.uptime().as_secs(),
        idle: [
            st.idle.to_frozen.as_secs(),
            st.idle.to_snapshot.as_secs(),
            st.idle.to_detached.as_secs(),
        ],
        tiers: by_tier,
        failed,
        snapshots,
        snapshot_bytes,
        templates,
        nodes: nodes
            .into_iter()
            .map(|n| ConsoleNode {
                last_seen_age: (now - n.last_seen_at).max(0),
                tiers: tiers_by_node.remove(&n.id).unwrap_or_default(),
                features: features.get(&n.id).cloned().unwrap_or_default(),
                id: n.id,
                pool: n.pool,
                connected: n.connected,
                free_mib: n.free_mib,
                psi: n.psi_some_avg10,
                max_sandboxes: n.max_sandboxes,
            })
            .collect(),
        failing: failing
            .into_iter()
            .map(|(id, node, error, attempts)| ConsoleFailing {
                id,
                node,
                error,
                attempts,
            })
            .collect(),
        counters,
    }))
}

/// The header a sandbox's own access token travels in.
///
/// A header, not the body: `ping` is a GET, and a query parameter would put
/// the secret in logs and in `Referer`.
pub const SANDBOX_TOKEN_HEADER: &str = "x-forkd-sandbox-token";

/// Check the caller holds *this sandbox's* token (RFC 16.5, layer two).
///
/// The API token is one credential for the whole deployment: holding it
/// means holding every sandbox. This is the second layer -- "who may enter
/// this sandbox" as distinct from "who may be a node" -- and it is why
/// knowing a sandbox id is no longer enough to reach into it.
///
/// Mandatory by design. v0.5's traffic token was opt-in, so every sandbox
/// made before anyone turned it on was open; the RFC removes that mode
/// rather than defaulting it on.
fn require_sandbox_token(st: &St, id: &str, headers: &axum::http::HeaderMap) -> Result<(), ApiErr> {
    let expected = {
        let store = st.store();
        store
            .get_sandbox(id)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            .map(|s| s.token)
    };
    let Some(expected) = expected else {
        return Err(err(StatusCode::NOT_FOUND, format!("no sandbox {id}")));
    };
    let presented = headers
        .get(SANDBOX_TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if crate::auth::secrets_match(presented, &expected) {
        return Ok(());
    }
    // 403 rather than 404: the caller already proved it may talk to this
    // API, and pretending the sandbox does not exist would only make a
    // missing header look like a bug in their code.
    Err(err(
        StatusCode::FORBIDDEN,
        format!(
            "this sandbox requires its own access token in the \
             {SANDBOX_TOKEN_HEADER} header; it is the `token` field of the \
             response that created the sandbox"
        ),
    ))
}

/// The admin panel: one file, served from this binary.
///
/// Open, like any login page -- it holds no secrets. The API token is typed
/// into it and kept in that tab's sessionStorage, so it never reaches disk
/// through the panel and is gone when the tab closes. Every call the page
/// makes goes through the same guarded API as any other client.
///
/// Built into the binary rather than shipped separately so the panel is
/// always the version of the master it is talking to, and so a single-box
/// deployment stays a single file.
async fn panel() -> impl IntoResponse {
    (
        [("content-type", "text/html; charset=utf-8")],
        include_str!("panel.html"),
    )
}

/// The join script, with this master's address filled in.
///
/// `POST /v1/node-bootstrap` hands out a one-liner that fetches this. It
/// used to name `/install.sh` without anything serving it, so the command
/// the panel produced returned 404 -- the endpoint existed and its output
/// could not work.
async fn install_script(headers: axum::http::HeaderMap) -> impl IntoResponse {
    // The master cannot know its own external address -- it may be bound to
    // 0.0.0.0 behind a NAT -- so the URL comes from the Host the caller
    // used, which is by definition one that reaches it.
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("127.0.0.1:8080");
    let body = include_str!("../../../../packaging/install.sh")
        .replace("@MASTER@", &format!("https://{host}"));
    (
        [("content-type", "text/x-shellscript; charset=utf-8")],
        body,
    )
}

/// The CA certificate, so the join script can check it against the
/// fingerprint its token carries. A certificate is a public key; serving it
/// gives away nothing that verifying it does not already require.
async fn ca_pem(State(st): State<St>) -> impl IntoResponse {
    (
        [("content-type", "application/x-pem-file")],
        st.ca.lock().cert_pem(),
    )
}

/// This master's own executable.
///
/// A node runs the same build as its master, which is the simplest way to
/// keep them from drifting -- and it means joining a machine needs no
/// release infrastructure and no internet, which matters because the
/// machines forkd is developed on have none.
///
/// The binary is not a secret: forkd is open source and the registration
/// token, not obscurity, is what gates joining.
async fn own_binary() -> Result<impl IntoResponse, ApiErr> {
    let bytes = std::fs::read("/proc/self/exe").map_err(|e| {
        err(
            StatusCode::NOT_IMPLEMENTED,
            format!("this master cannot serve its own binary: {e}"),
        )
    })?;
    Ok((
        [
            ("content-type", "application/octet-stream"),
            (
                "content-disposition",
                "attachment; filename=\"forkd-serve\"",
            ),
        ],
        bytes,
    ))
}

/// What the transport established about the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeIdentity {
    /// The connection presented a certificate the master issued to this node.
    Verified(String),
    /// The control plane is running without TLS, so there is nothing to
    /// verify. Kept as a distinct case rather than collapsing into
    /// `Verified(whatever the body claimed)` so that nothing downstream can
    /// mistake an unverified id for a proven one.
    Untrusted,
}

impl NodeIdentity {
    /// Check the id a request claims against the connection it came in on.
    ///
    /// Under TLS a node holding a valid certificate still cannot act as a
    /// different node; without TLS this can only wave the claim through,
    /// which is why `--insecure-transport` logs what it gives up.
    pub fn authorises(&self, claimed: &str) -> bool {
        match self {
            Self::Verified(id) => id == claimed,
            Self::Untrusted => true,
        }
    }
}

/// Resolve the client certificate on this connection to a node.
async fn require_node_cert(
    st: St,
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Result<axum::response::Response, ApiErr> {
    if !st.require_node_certs {
        req.extensions_mut().insert(NodeIdentity::Untrusted);
        return Ok(next.run(req).await);
    }
    let presented = req
        .extensions()
        .get::<crate::tls::PeerCert>()
        .and_then(|p| p.0.clone());
    let Some(fp) = presented else {
        return Err(err(
            StatusCode::UNAUTHORIZED,
            "this endpoint requires a node client certificate; register with              `forkd node-token` to get one",
        ));
    };
    // Resolved into a value first: a lock guard held as the match scrutinee
    // would still be alive across `next.run(req).await` below.
    let owner = {
        let store = st.store();
        store
            .node_by_cert(&fp)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    };
    match owner {
        crate::state::CertOwner::Node(id) => {
            req.extensions_mut().insert(NodeIdentity::Verified(id));
            Ok(next.run(req).await)
        }
        crate::state::CertOwner::Revoked(id) => Err(err(
            StatusCode::FORBIDDEN,
            format!("the certificate for node {id} is revoked"),
        )),
        crate::state::CertOwner::Unknown => Err(err(
            StatusCode::UNAUTHORIZED,
            "this client certificate belongs to no registered node",
        )),
    }
}

/// Revoke a node's certificate. The node keeps its files; the master stops
/// accepting them, which is enough because the node is the side that dials.
async fn revoke_node(
    State(st): State<St>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    let gone = st
        .store()
        .revoke_node(&id)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if !gone {
        return Err(err(StatusCode::NOT_FOUND, format!("no node {id}")));
    }
    // Wake its parked poll so the refusal lands now rather than when the
    // hold expires -- and if the master is the one connecting, stop.
    st.wake_node(&id);
    st.stop_driver(&id);
    tracing::warn!(node = %id, "revoked");
    Ok(Json(serde_json::json!({ "revoked": id })))
}

pub(super) fn err(
    code: StatusCode,
    msg: impl Into<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    (code, Json(serde_json::json!({ "error": msg.into() })))
}
pub(super) type ApiErr = (StatusCode, Json<serde_json::Value>);

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
    // Nodes and their loads are read under one lock, so the panel cannot
    // show a node refused against a count taken after someone else's delete.
    let nodes = {
        let store = st.store();
        let nodes = store
            .list_nodes()
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let mut out = Vec::with_capacity(nodes.len());
        for n in nodes {
            let load = store
                .count_sandboxes_on_node(&n.id)
                .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            out.push((n, load));
        }
        out
    };
    Ok(Json(
        nodes
            .into_iter()
            .map(|(n, load)| NodeView {
                not_admitting: st.admit.refuse(&n, load),
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

// ---- adding a node by address ----------------------------------------------

/// Port a listening node is reached on when the address does not say.
///
/// 80 because that is what the first fleet had open: its security group
/// allowed 22 and 80 inbound and dropped everything else, and nothing was
/// using 80 on the nodes. Any port works; this is only the default.
const AGENT_DEFAULT_PORT: u16 = 80;

#[derive(Deserialize)]
struct AddNode {
    /// `host` or `host:port` the master can reach. IPv6 as `[addr]:port`.
    address: String,
    /// What the node wrote to `pki/node/pairing-code` on first start.
    pairing_code: String,
    /// Defaults to the id the node reports, its `--node-id` or hostname.
    #[serde(default)]
    node_id: Option<String>,
    #[serde(default = "default_pool")]
    pool: String,
}

#[derive(Serialize)]
struct AddedNode {
    id: String,
    pool: String,
    address: String,
    fingerprint: String,
    max_sandboxes: u32,
}

fn normalise_address(raw: &str) -> Result<String, ApiErr> {
    let a = raw.trim();
    let a = a
        .strip_prefix("https://")
        .unwrap_or(a)
        .trim_end_matches('/');
    let bad = |why: &str| err(StatusCode::BAD_REQUEST, format!("address {raw:?}: {why}"));
    if a.is_empty() || a.contains('/') || a.chars().any(char::is_whitespace) {
        return Err(bad("expected host or host:port"));
    }
    if a.starts_with('[') {
        return Ok(if a.contains("]:") {
            a.to_string()
        } else {
            format!("{a}:{AGENT_DEFAULT_PORT}")
        });
    }
    match a.matches(':').count() {
        0 => Ok(format!("{a}:{AGENT_DEFAULT_PORT}")),
        1 => Ok(a.to_string()),
        _ => Err(bad("write an IPv6 address as [addr] or [addr]:port")),
    }
}

/// Take over a node that is listening.
///
/// The node is already installed and running with `--listen`; this is the
/// whole of joining it. The master connects, checks the certificate against
/// the pairing code, proves it holds the secret, learns what the node is,
/// and from then on drives it -- no token for the node to fetch, no URL for
/// it to be told, nothing to run on the node.
async fn add_node(
    State(st): State<St>,
    Json(req): Json<AddNode>,
) -> Result<(StatusCode, Json<AddedNode>), ApiErr> {
    let (pin, secret) = crate::tls::split_token(req.pairing_code.trim());
    let Some(pin) = pin.filter(|p| p.len() == 64 && p.bytes().all(|b| b.is_ascii_hexdigit()))
    else {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "pairing_code should be <64 hex>.<secret>, as the node writes it to \
             pki/node/pairing-code",
        ));
    };
    if secret.is_empty() {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "pairing_code has no secret part",
        ));
    }
    let pin = pin.to_ascii_lowercase();
    let address = normalise_address(&req.address)?;

    let about = super::drive::hello(&address, &pin, secret)
        .await
        .map_err(|e| {
            err(
                StatusCode::BAD_GATEWAY,
                format!("could not take over {address}: {e:#}"),
            )
        })?;
    let node_id = req.node_id.unwrap_or(about.node_id);
    if !crate::tag::is_safe(&node_id) {
        return Err(err(
            StatusCode::BAD_REQUEST,
            format!("node id {node_id:?} is not a plain name"),
        ));
    }

    // Two drivers for one node -- this one, and the node itself still
    // polling under the same id -- would each be handed its sandboxes.
    {
        let store = st.store();
        let polling_now = store
            .list_nodes()
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            .into_iter()
            .any(|n| n.id == node_id && n.connected && now_secs() - n.last_seen_at < 60);
        let already_ours = store
            .agent_nodes()
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            .iter()
            .any(|a| a.id == node_id);
        if polling_now && !already_ours {
            return Err(err(
                StatusCode::CONFLICT,
                format!(
                    "node {node_id} is connected and polling right now; stop its \
                     pull-mode service first, or add this one under another node_id"
                ),
            ));
        }
    }

    let pool = pool_for(&st, req.pool, &about.fingerprint, &node_id)?;
    {
        let store = st.store();
        store
            .upsert_node(&Node {
                id: node_id.clone(),
                pool: pool.clone(),
                fingerprint: about.fingerprint.clone(),
                max_sandboxes: about.max_sandboxes,
                last_seen_at: now_secs(),
                // Connected once the driver's first sync lands, not before.
                connected: false,
                free_mib: 0,
                psi_some_avg10: 0.0,
            })
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        store
            .set_node_agent(&node_id, &address, &pin, secret)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    super::drive::start(
        st.clone(),
        crate::state::AgentNode {
            id: node_id.clone(),
            address: address.clone(),
            cert_sha256: pin,
            secret: secret.to_string(),
        },
    );
    tracing::info!(node = %node_id, %address, pool = %pool, "node added; the master drives it");
    Ok((
        StatusCode::CREATED,
        Json(AddedNode {
            id: node_id,
            pool,
            address,
            fingerprint: about.fingerprint,
            max_sandboxes: about.max_sandboxes,
        }),
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
    let secret = random_token();
    st.store()
        .create_node_token(&secret, &req.pool, req.ttl_secs)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    // Handed out with the CA fingerprint so the node can verify the master
    // on the one request that carries this secret.
    let token = st
        .ca_pin()
        .map(|pin| crate::tls::join_token(&pin, &secret))
        .unwrap_or(secret);
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
    /// The CA's SHA-256, which is also the first half of `token`.
    ///
    /// Shown separately so an operator can compare it out of band, or pass
    /// the CA to the script with `FORKD_CA=` and skip the unverified first
    /// fetch entirely.
    #[serde(skip_serializing_if = "Option::is_none")]
    ca_fingerprint: Option<String>,
    /// Said in the response rather than left to be discovered: the first
    /// fetch of the script cannot be verified, because the fingerprint that
    /// would verify it is inside the script's own argument. Everything the
    /// script does afterwards is checked against that fingerprint.
    first_fetch_is_unverified: bool,
}

/// Mint a token and render the one line that joins a new machine.
async fn node_bootstrap(
    State(st): State<St>,
    Json(req): Json<BootstrapReq>,
) -> Result<Json<BootstrapView>, ApiErr> {
    let secret = random_token();
    st.store()
        .create_node_token(&secret, &req.pool, req.ttl_secs)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let token = st
        .ca_pin()
        .map(|pin| crate::tls::join_token(&pin, &secret))
        .unwrap_or(secret);
    let master = req.master_url.trim_end_matches('/');
    let pin = st.ca_pin();
    // `-k` on this one hop only. The master's certificate is signed by its
    // own CA, so plain curl refuses it, and the fingerprint that would
    // verify it is carried in the token the command is passing. The script
    // fetches the CA, checks it against that fingerprint, and does
    // everything after with --cacert. An operator who wants the first hop
    // verified too copies pki/ca.pem over and sets FORKD_CA.
    Ok(Json(BootstrapView {
        install_command: format!(
            "curl -sSfL -k {master}/install.sh | sudo FORKD_MASTER_URL={master} \\\n    FORKD_NODE_TOKEN={token} sh"
        ),
        token,
        pool: req.pool,
        expires_at: now_secs() + req.ttl_secs,
        ca_fingerprint: pin,
        first_fetch_is_unverified: true,
    }))
}

/// Which pool a node with this host fingerprint actually joins.
///
/// A pool is a migration domain, not a label: every node in it must be able
/// to restore every other's checkpoints, and runsc refuses across even a
/// one-week release difference. So a machine whose fingerprint does not
/// match the pool's does not join it -- it gets a pool of its own, derived
/// from the fingerprint. The alternative is a pool that mixes two versions
/// and only says so when a restore fails, which is the failure mode RFC
/// section 15 set out to avoid.
///
/// The same rule whether a node registers or is added by address.
pub(super) fn pool_for(
    st: &St,
    asked: String,
    fingerprint: &str,
    node_id: &str,
) -> Result<String, ApiErr> {
    let store = st.store();
    let seen = store
        .pool_fingerprints(&asked)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let mismatched: Vec<&String> = seen.iter().filter(|f| *f != fingerprint).collect();
    if mismatched.is_empty() {
        return Ok(asked);
    }
    let derived = format!("{asked}#{}", &fingerprint[..8.min(fingerprint.len())]);
    tracing::warn!(
        node = %node_id,
        asked = %asked,
        joined = %derived,
        fingerprint = %fingerprint,
        others = ?mismatched,
        "fingerprint does not match this pool; the node joins a derived pool \
         instead so its snapshots are never scheduled onto a host that cannot \
         restore them"
    );
    Ok(derived)
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
        let store = st.store();
        store
            // Only the secret half is stored; the pin is public.
            .redeem_node_token(crate::tls::split_token(&req.token).1, &req.node_id)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    };
    let Some(pool) = pool else {
        return Err(err(
            StatusCode::UNAUTHORIZED,
            "registration token is unknown, expired, or already used",
        ));
    };

    let pool = pool_for(&st, pool, &req.fingerprint, &req.node_id)?;

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

    // Tie this certificate to this node before the node can use it, so the
    // very next request already resolves through it.
    let cert_fp = crate::tls::fingerprint_of_pem(&cert)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    st.store()
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
    st.store()
        .set_node_cert(&req.node_id, &cert_fp)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    tracing::info!(node = %req.node_id, pool = %pool, cert = %cert_fp, "node registered");
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
    Extension(who): Extension<NodeIdentity>,
    Json(req): Json<PollRequest>,
) -> Result<Json<PollResponse>, ApiErr> {
    // The certificate decides which node this is, not the body. Otherwise a
    // node holding any valid certificate could poll as another node and be
    // handed its sandboxes -- and its exec output.
    if !who.authorises(&req.node_id) {
        tracing::warn!(
            claimed = %req.node_id,
            identity = ?who,
            "refused a poll claiming a node id the certificate does not match"
        );
        return Err(err(
            StatusCode::FORBIDDEN,
            "this certificate is not registered for that node id",
        ));
    }
    // Held open, not polled. The node dials in and this request stays
    // parked until there is work for it, so dispatch costs one round trip
    // rather than up to a poll interval -- the operator-visible behaviour
    // of a machine that accepts scheduling the moment its service is up,
    // without the node listening on anything.
    record_failures(&st, &req)?;
    apply_traffic(&st, &req)?;

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

/// A report out of turn, sent by a polling node the moment its gateway
/// woke a sandbox.
///
/// The node's regular report rides on its next poll, and the one it has
/// parked here can sit for the whole hold -- 25 s by default. Until then the
/// master shows the old tier, and an explicit request for that same tier is
/// a no-op because desired state already says so (measured on bm: a freeze
/// sent 16 s after a wake did nothing). This closes that window to a round
/// trip. It returns no commands, so it cannot race the parked poll for them.
async fn node_traffic(
    State(st): State<St>,
    Extension(who): Extension<NodeIdentity>,
    Json(req): Json<PollRequest>,
) -> Result<StatusCode, ApiErr> {
    if !who.authorises(&req.node_id) {
        return Err(err(
            StatusCode::FORBIDDEN,
            "this certificate is not registered for that node id",
        ));
    }
    apply_traffic(&st, &req)?;
    // The parked poll diffs against the report it arrived with, from before
    // the wake. Wake it so the node comes back with a current one.
    st.wake_node(&req.node_id);
    Ok(StatusCode::NO_CONTENT)
}

pub(super) fn take_execs(st: &St, node_id: &str) -> Vec<Command> {
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
        let store = st.store();
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
        let store = st.store();
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
    headers: axum::http::HeaderMap,
    Json(req): Json<ExecReq>,
) -> Result<Json<ExecView>, ApiErr> {
    require_sandbox_token(&st, &id, &headers)?;
    if req.args.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "args must not be empty"));
    }
    let r = run_exec(&st, &id, req.args, req.timeout_secs).await?;
    Ok(Json(ExecView {
        stdout: r.stdout,
        stderr: r.stderr,
        exit_code: r.exit_code,
    }))
}

/// Queue an exec for the sandbox's node and wait for the result. Shared by
/// `exec` and `eval`, which differ only in how they build the command and
/// shape the answer.
async fn run_exec(
    st: &St,
    id: &str,
    args: Vec<String>,
    timeout_secs: u64,
) -> Result<CommandResult, ApiErr> {
    let (node_id, tier) = {
        let store = st.store();
        let Some(sb) = store
            .get_sandbox(id)
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
                id: id.to_string(),
                args,
                timeout_secs,
            });
    }
    st.wake_node(&node_id);

    // The node's own timeout plus room for a round trip and a loaded node.
    let budget = Duration::from_secs(timeout_secs.saturating_add(20));
    match tokio::time::timeout(budget, rx).await {
        Ok(Ok(r)) => Ok(r),
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

/// The node reporting the outcome of a command someone is waiting on.
/// Hand an exec or branch result to whoever is waiting on it. False when
/// nobody is: the caller timed out, or the id was never issued.
pub(super) fn deliver_result(st: &St, r: CommandResult) -> bool {
    match st.pending_exec.lock().remove(&r.result_id) {
        Some(tx) => {
            // The caller may already have timed out and gone.
            let _ = tx.send(r);
            true
        }
        None => false,
    }
}

async fn command_result(
    State(st): State<St>,
    Json(r): Json<CommandResult>,
) -> Result<StatusCode, ApiErr> {
    if deliver_result(&st, r) {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(err(
            StatusCode::NOT_FOUND,
            "no caller is waiting on that result",
        ))
    }
}

#[derive(Deserialize)]
struct EvalReq {
    code: String,
    #[serde(default = "default_exec_timeout")]
    timeout_secs: u64,
}

/// The v0.5 shape: a result or an error, never both.
#[derive(Serialize)]
struct EvalView {
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    exit_code: i64,
}

/// Evaluate a snippet in a sandbox.
///
/// A thin layer over exec: the template says how to run code
/// (`eval_argv`, `python3 -c` by default) and the snippet is appended.
/// The v0.5 controller reached an agent inside the guest for this; with
/// gVisor there is no agent to reach.
async fn eval_sandbox(
    State(st): State<St>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
    Json(req): Json<EvalReq>,
) -> Result<Json<EvalView>, ApiErr> {
    require_sandbox_token(&st, &id, &headers)?;
    let argv = {
        let store = st.store();
        let Some(sb) = store
            .get_sandbox(&id)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        else {
            return Err(err(StatusCode::NOT_FOUND, "no such sandbox"));
        };
        let spec = store
            .get_template(&sb.template)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            .and_then(|(_r, s)| serde_json::from_str::<serde_json::Value>(&s).ok());
        let mut argv: Vec<String> = spec
            .as_ref()
            .and_then(|v| v.get("eval_argv"))
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_else(|| vec!["/usr/bin/python3".to_string(), "-c".to_string()]);
        argv.push(req.code);
        argv
    };

    let r = run_exec(&st, &id, argv, req.timeout_secs).await?;
    Ok(Json(if r.exit_code == 0 {
        EvalView {
            result: Some(r.stdout),
            error: None,
            exit_code: 0,
        }
    } else {
        EvalView {
            result: None,
            error: Some(if r.stderr.is_empty() {
                r.stdout
            } else {
                r.stderr
            }),
            exit_code: r.exit_code,
        }
    }))
}

#[derive(Serialize)]
struct PingView {
    ok: bool,
    tier: Tier,
    #[serde(skip_serializing_if = "Option::is_none")]
    guest_addr: Option<String>,
    /// How long the check took, so a caller can see a slow node.
    latency_ms: u64,
}

/// Is this sandbox actually answering?
///
/// Reads desired state and then opens a TCP connection to the sandbox's
/// own address. A sandbox that is `hot` in the master's view but not
/// accepting connections reports `ok: false` rather than being taken on
/// trust -- the whole design holds that actual state is observed, never
/// remembered.
async fn ping_sandbox(
    State(st): State<St>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
) -> Result<Json<PingView>, ApiErr> {
    require_sandbox_token(&st, &id, &headers)?;
    let t0 = std::time::Instant::now();
    let (tier, addr) = {
        let store = st.store();
        let Some(sb) = store
            .get_sandbox(&id)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        else {
            return Err(err(StatusCode::NOT_FOUND, "no such sandbox"));
        };
        let addr = match (sb.ip.as_deref(), sb.port) {
            (Some(ip), Some(port)) => Some(format!("{ip}:{port}")),
            _ => None,
        };
        (sb.tier, addr)
    };

    let ok = match (&addr, tier) {
        (Some(a), Tier::Hot) => {
            let a = a.clone();
            tokio::time::timeout(Duration::from_secs(3), tokio::net::TcpStream::connect(a))
                .await
                .map(|r| r.is_ok())
                .unwrap_or(false)
        }
        // Anything not hot has no process to answer, which is correct
        // rather than broken.
        _ => false,
    };
    Ok(Json(PingView {
        ok,
        tier,
        guest_addr: addr,
        latency_ms: t0.elapsed().as_millis() as u64,
    }))
}

#[derive(Deserialize)]
struct BranchReq {
    /// Name for the new snapshot.
    tag: String,
    /// Removed in v0.6. Present so the refusal can name it.
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    wait: Option<bool>,
    #[serde(default)]
    live: Option<bool>,
    /// Firecracker diff-snapshot measurement. Refused rather than ignored:
    /// gVisor has no diff snapshots, so the numbers it asked for cannot be
    /// produced, and quietly dropping the flag would have the caller
    /// believe otherwise.
    #[serde(default)]
    diff: Option<bool>,
    #[serde(default)]
    measure_diff: Option<bool>,
}

/// A snapshot as the API serves it, with the v0.5 names the SDKs read:
/// `branched_from` for the sandbox it came from, `created_at_unix` for its
/// age, and `dir` for where the image lives.
#[derive(Serialize)]
struct SnapshotView {
    tag: String,
    node_id: String,
    template: String,
    /// v0.5 name for `from_sandbox`.
    #[serde(skip_serializing_if = "Option::is_none")]
    branched_from: Option<String>,
    created_at_unix: i64,
    dir: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    from_sandbox: Option<String>,
    created_at: i64,
    size_bytes: u64,
}

impl From<SnapshotTag> for SnapshotView {
    fn from(s: SnapshotTag) -> Self {
        Self {
            branched_from: s.from_sandbox.clone(),
            created_at_unix: s.created_at,
            dir: format!("tags/{}", s.tag),
            tag: s.tag,
            node_id: s.node_id,
            template: s.template,
            from_sandbox: s.from_sandbox,
            created_at: s.created_at,
            size_bytes: s.size_bytes,
        }
    }
}

/// Checkpoint a running sandbox into a named tag it can be forked from.
///
/// The parent keeps running: runsc's checkpoint stops the container, so the
/// node restores it from the image it just wrote. Measured on bm that is
/// 33 ms to checkpoint and 45 ms to restore, so the parent pauses for well
/// under a tenth of a second. A branch with no pause at all is an RFC
/// non-goal, and this is the honest version of that trade.
async fn branch_sandbox(
    State(st): State<St>,
    Path(id): Path<String>,
    Json(req): Json<BranchReq>,
) -> Result<(StatusCode, Json<SnapshotView>), ApiErr> {
    if req.mode.as_deref() == Some("live") || req.live == Some(true) {
        return Err(err(
            StatusCode::GONE,
            "mode=\"live\" is removed in v0.6. A branch is a checkpoint \
             restored into each child; the parent pauses for about 80 ms. \
             Drop the field for the same call.",
        ));
    }
    if req.wait.is_some() {
        return Err(err(
            StatusCode::GONE,
            "wait is removed in v0.6: branch always waits, and returns the \
             snapshot once the node has written it. Drop the field.",
        ));
    }
    if req.diff == Some(true) || req.measure_diff == Some(true) {
        return Err(err(
            StatusCode::GONE,
            "diff and measure_diff are removed in v0.6: they measured \
             Firecracker diff snapshots, and gVisor has none. The response \
             carries size_bytes for the whole image instead. Drop both.",
        ));
    }
    if !crate::tag::is_safe(&req.tag) {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "tag must be a plain name: no slashes, no dots at the start",
        ));
    }

    let (node_id, template, tier) = {
        let store = st.store();
        if store
            .get_snapshot(&req.tag)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            .is_some()
        {
            return Err(err(
                StatusCode::CONFLICT,
                format!("snapshot {} already exists", req.tag),
            ));
        }
        let Some(sb) = store
            .get_sandbox(&id)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        else {
            return Err(err(StatusCode::NOT_FOUND, "no such sandbox"));
        };
        (sb.node_id, sb.template, sb.tier)
    };

    // Only a running sandbox has memory to checkpoint.
    if tier != Tier::Hot {
        return Err(err(
            StatusCode::CONFLICT,
            format!("sandbox is {tier}; POST /v1/sandboxes/{id}/touch to bring it hot first"),
        ));
    }

    let result_id = random_token();
    let (tx, rx) = oneshot::channel();
    {
        st.pending_exec.lock().insert(result_id.clone(), tx);
        st.exec_queue
            .lock()
            .entry(node_id.clone())
            .or_default()
            .push_back(Command::Branch {
                id: id.clone(),
                tag: req.tag.clone(),
                result_id: result_id.clone(),
            });
    }
    st.wake_node(&node_id);

    let budget = Duration::from_secs(180);
    let r = match tokio::time::timeout(budget, rx).await {
        Ok(Ok(r)) => r,
        _ => {
            st.pending_exec.lock().remove(&result_id);
            return Err(err(
                StatusCode::GATEWAY_TIMEOUT,
                format!("node {node_id} did not finish the branch within {budget:?}"),
            ));
        }
    };
    if r.exit_code != 0 {
        return Err(err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("branch failed on {node_id}: {}", r.stderr.trim()),
        ));
    }

    // Recorded only once the node has actually written the image, so a fork
    // from this tag cannot race ahead of it.
    let snap = SnapshotTag {
        tag: req.tag,
        node_id,
        template,
        from_sandbox: Some(id),
        created_at: now_secs(),
        size_bytes: r.size_bytes,
    };
    {
        let store = st.store();
        store
            .put_snapshot(&snap)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        // Which host this image can be restored on. runsc refuses across
        // even a one-week release difference, so the fingerprint is part of
        // what the tag IS, not metadata about it.
        if let Ok(Some(fp)) = store.node_fingerprint(&snap.node_id) {
            let _ = store.set_snapshot_fingerprint(&snap.tag, &fp);
        }
    }
    Ok((StatusCode::CREATED, Json(snap.into())))
}

async fn list_snapshots(State(st): State<St>) -> Result<Json<Vec<SnapshotView>>, ApiErr> {
    let all = st
        .store()
        .list_snapshots()
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(all.into_iter().map(Into::into).collect()))
}

#[derive(Serialize)]
struct SnapshotInfo {
    #[serde(flatten)]
    snapshot: SnapshotView,
    /// Sandboxes still forked from it. A tag with children is in use.
    children: u32,
}

async fn snapshot_info(
    State(st): State<St>,
    Path(tag): Path<String>,
) -> Result<Json<SnapshotInfo>, ApiErr> {
    let store = st.store();
    let Some(snap) = store
        .get_snapshot(&tag)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    else {
        return Err(err(StatusCode::NOT_FOUND, format!("no snapshot {tag}")));
    };
    let children = store
        .children_of(&tag)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(SnapshotInfo {
        snapshot: snap.into(),
        children,
    }))
}

/// Drop a tag. Refused while sandboxes are still forked from it: one image
/// serves every child, so removing it under them would strand them.
async fn delete_snapshot(
    State(st): State<St>,
    Path(tag): Path<String>,
) -> Result<StatusCode, ApiErr> {
    let store = st.store();
    let children = store
        .children_of(&tag)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if children > 0 {
        return Err(err(
            StatusCode::CONFLICT,
            format!("{children} sandboxes are still forked from {tag}"),
        ));
    }
    if !store
        .delete_snapshot(&tag)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    {
        return Err(err(StatusCode::NOT_FOUND, format!("no snapshot {tag}")));
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Keep the reason a sandbox is not coming up where an operator can see it.
pub(super) fn record_failures(st: &St, req: &PollRequest) -> Result<(), ApiErr> {
    if req.failures.is_empty() {
        return Ok(());
    }
    let store = st.store();
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

/// Requests the node's gateway forwarded since it last reported.
///
/// The gateway wakes a cold sandbox on its own, so by the time this arrives
/// the sandbox may already be hot against a desired tier that says
/// otherwise. Making Hot the desired tier here, before `diff`, is what stops
/// the next diff from putting it straight back to sleep. And every request
/// moves the idle clock, which nothing did for gateway traffic before: a
/// sandbox serving requests on its port was demoted as idle on schedule.
///
/// Only the node holding the sandbox is believed. Any node can put any id
/// in its report, and one that does not hold the sandbox has no business
/// keeping it awake.
///
/// And traffic only promotes a sandbox this same report shows serving --
/// one that really was woken. A polling node can sit on a report for a
/// whole hold, so traffic can arrive after the node has, quite rightly,
/// demoted the sandbox on a later command once it had gone quiet. Found on
/// bm: a freeze issued 16 s after the last request was undone at once by
/// that request's late report. Late traffic still moves the idle clock;
/// it just does not overrule a demotion that came after it.
pub(super) fn apply_traffic(st: &St, req: &PollRequest) -> Result<(), ApiErr> {
    if req.traffic.is_empty() {
        return Ok(());
    }
    let store = st.store();
    let ise = |e: anyhow::Error| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    for id in &req.traffic {
        let Some(sb) = store.get_sandbox(id).map_err(ise)? else {
            continue;
        };
        if sb.node_id != req.node_id {
            tracing::warn!(sandbox = %id, node = %req.node_id, holder = %sb.node_id, "ignored traffic reported by a node that does not hold the sandbox");
            continue;
        }
        store.touch(id).map_err(ise)?;
        let serving = req
            .observed
            .iter()
            .any(|o| &o.id == id && o.tier.is_serving());
        if serving && sb.tier != Tier::Hot {
            tracing::info!(sandbox = %id, node = %req.node_id, was = %sb.tier, "woken by traffic at its gateway");
            store.set_tier(id, Tier::Hot).map_err(ise)?;
            st.metrics.woken.fetch_add(1, Relaxed);
        }
    }
    Ok(())
}

/// Desired state minus what the node reports holding. Sync and short, so the
/// store lock is never held across an await.
pub(super) fn diff(st: &St, req: &PollRequest) -> Result<Vec<Command>, ApiErr> {
    let store = st.store();
    store
        .set_node_health(&req.node_id, req.free_memory_mib, req.pressure_some_avg10)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    // An empty list from an early wake report says nothing; keep the last.
    if !req.features.is_empty() {
        st.metrics
            .node_features
            .lock()
            .insert(req.node_id.clone(), req.features.clone());
    }

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
                        let mut v: serde_json::Value =
                            serde_json::from_str(&spec).unwrap_or(serde_json::Value::Null);
                        // The caller's ceiling wins over the template's.
                        // Applied here rather than stored in the template,
                        // so one template can serve sandboxes of different
                        // sizes.
                        if let (Some(mib), Some(obj)) = (want.memory_limit_mib, v.as_object_mut()) {
                            obj.insert("memory_max_mib".to_string(), serde_json::Value::from(mib));
                        }
                        v
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
                    from_tag: want.from_tag.clone(),
                    gateway_port: want.gateway_port,
                    token: want.token.clone(),
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

/// The sandbox as the API serves it.
///
/// Carries the v0.5 names alongside the v0.6 ones, because both SDKs read
/// the old ones: `snapshot_tag` for the tag it came from, `guest_addr` for
/// where to reach it, `created_at_unix` for its age. Duplication on the
/// wire is cheap; a field an SDK cannot find is not.
#[derive(Serialize)]
struct SandboxView {
    id: String,
    node_id: String,
    pool: String,
    template: String,
    tier: Tier,
    /// v0.5 name for the tag this sandbox was forked from.
    #[serde(skip_serializing_if = "Option::is_none")]
    snapshot_tag: Option<String>,
    /// v0.5 name for where to reach it, as `ip:port`.
    #[serde(skip_serializing_if = "Option::is_none")]
    guest_addr: Option<String>,
    /// v0.5 name for `created_at`.
    created_at_unix: i64,
    /// The network namespace it runs in, which v0.5 also reported.
    #[serde(skip_serializing_if = "Option::is_none")]
    netns: Option<String>,
    /// The ceiling actually in force: the caller's override when they gave
    /// one, absent when the template's applies.
    #[serde(skip_serializing_if = "Option::is_none")]
    memory_limit_mib: Option<u64>,
    volume: Option<String>,
    port: Option<u16>,
    /// Host port to reach this sandbox on, from off the node. Absent when
    /// none was free, in which case it is reachable only from its node.
    /// Requests here need the `token` below in `x-forkd-sandbox-token`.
    #[serde(skip_serializing_if = "Option::is_none")]
    gateway_port: Option<u16>,
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
            snapshot_tag: s.from_tag.clone(),
            guest_addr: match (s.ip.as_deref(), s.port) {
                (Some(ip), Some(port)) => Some(format!("{ip}:{port}")),
                _ => None,
            },
            created_at_unix: s.created_at,
            netns: Some(format!("forkd-{}", s.id)),
            memory_limit_mib: s.memory_limit_mib,
            id: s.id,
            node_id: s.node_id,
            pool: s.pool,
            template: s.template,
            tier: s.tier,
            volume: s.volume,
            port: s.port,
            gateway_port: s.gateway_port,
            ip: s.ip,
            created_at: s.created_at,
            last_active_at: s.last_active_at,
            last_error: s.last_error,
            error_attempts: s.error_attempts,
        }
    }
}

/// Same body the v0.5 controller took, so the SDKs keep working: fork `n`
/// children from `snapshot_tag`. `template` is the v0.6 way to start one
/// cold; exactly one of the two is required.
#[derive(Deserialize)]
struct NewSandbox {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    template: Option<String>,
    /// Fork from this tag. One image serves every child.
    #[serde(default)]
    snapshot_tag: Option<String>,
    /// How many to make. Its presence also selects the v0.5 response
    /// shape, which is a list.
    #[serde(default)]
    n: Option<u32>,
    #[serde(default)]
    pool: Option<String>,
    #[serde(default)]
    node_id: Option<String>,
    #[serde(default)]
    volume: Option<String>,
    #[serde(default)]
    port: Option<u16>,
    /// Per-sandbox memory ceiling in MiB, overriding the template's. The
    /// v0.5 name, which both SDKs send.
    #[serde(default)]
    memory_limit_mib: Option<u64>,
    /// Accepted and ignored, either way. Every sandbox gets its own
    /// network namespace in v0.6, so `true` asks for what already
    /// happens and `false` asks for something weaker than it will get.
    ///
    /// `false` was refused with a 410 at first, on the reasoning that a
    /// caller should not silently receive something other than it asked
    /// for. That was wrong in practice: both SDKs send `false` by
    /// DEFAULT, so the 410 broke `spawnSandboxes({snapshotTag})` and
    /// `spawn_sandboxes(tag)` -- the plainest call either one makes. A
    /// refusal is right when honouring is impossible and the caller would
    /// be misled; here the caller gets strictly stronger isolation, and
    /// nobody is harmed by not being able to ask for less.
    // Never read, deliberately: the field exists so the wire contract is
    // documented where someone looks for it, not because the value
    // changes anything.
    #[allow(dead_code)]
    #[serde(default)]
    per_child_netns: Option<bool>,
    /// Removed in v0.6. Present only so the refusal can name it.
    #[serde(default)]
    live_fork: Option<bool>,
    #[serde(default)]
    hugepages: Option<bool>,
}

async fn create_sandbox(
    State(st): State<St>,
    Json(req): Json<NewSandbox>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiErr> {
    // Removed in v0.6. Named in the refusal with what replaces it, as the
    // RFC's compatibility section requires, rather than ignored.
    if req.live_fork == Some(true) || req.hugepages == Some(true) {
        return Err(err(
            StatusCode::GONE,
            "live_fork and hugepages are removed in v0.6; a branch is a \
             checkpoint restored into each child, so there is no memfd to \
             configure. Drop both fields.",
        ));
    }

    // Refused here, not on a node. The node validates again before it
    // touches the filesystem -- that is what stops traversal -- but letting
    // a bad tag through means it is written to the database and then fails
    // on a node with a message about a missing directory. Validating in the
    // handler that joins a path and nowhere else is what the 2026-05-17
    // advisory was about.
    if let Some(t) = req.snapshot_tag.as_deref() {
        if !crate::tag::is_safe(t) {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "snapshot_tag must be a plain name: no path separators, no \
                 leading dot, no \"..\"",
            ));
        }
    }
    let (template, from_tag) = match (&req.template, &req.snapshot_tag) {
        (Some(_), Some(_)) => {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "give either template or snapshot_tag, not both",
            ))
        }
        (None, None) => {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "one of template or snapshot_tag is required",
            ))
        }
        (Some(t), None) => (t.clone(), None),
        (None, Some(tag)) => (String::new(), Some(tag.clone())),
    };

    let count = req.n.unwrap_or(1);
    if count == 0 {
        return Err(err(StatusCode::BAD_REQUEST, "n must be at least 1"));
    }
    if count > MAX_FORK_WIDTH {
        return Err(err(
            StatusCode::BAD_REQUEST,
            format!("n must be at most {MAX_FORK_WIDTH}"),
        ));
    }
    // An explicit id cannot name more than one sandbox.
    if req.id.is_some() && count > 1 {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "id cannot be given with n > 1",
        ));
    }
    // A node hides its own seed sandboxes by this prefix, so a sandbox named
    // with it would be invisible to the node that runs it.
    if req
        .id
        .as_deref()
        .is_some_and(|i| i.starts_with(crate::node::WARM_SEED_PREFIX))
    {
        return Err(err(
            StatusCode::BAD_REQUEST,
            format!(
                "ids starting with {} are reserved",
                crate::node::WARM_SEED_PREFIX
            ),
        ));
    }

    let store = st.store();

    // A fork is pinned to the node holding the image: tags are node-local,
    // and a checkpoint only restores where it was taken.
    let (template, pool, node_id) = match &from_tag {
        Some(tag) => {
            let Some(snap) = store
                .get_snapshot(tag)
                .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            else {
                return Err(err(StatusCode::NOT_FOUND, format!("no snapshot {tag}")));
            };
            // A tag lives in `work_dir/tags` on the node that baked it, so
            // a fork runs there. The only way to land elsewhere is for the
            // caller to name a node, and that is worth refusing at schedule
            // time rather than letting the restore fail on the node: a
            // mismatched fingerprint means runsc will not accept the image.
            if let Some(asked) = req.node_id.as_deref() {
                if asked != snap.node_id {
                    let tag_fp = store.snapshot_fingerprint(tag).ok().flatten();
                    let node_fp = store.node_fingerprint(asked).ok().flatten();
                    if let (Some(t), Some(n)) = (&tag_fp, &node_fp) {
                        if t != n {
                            return Err(err(
                                StatusCode::CONFLICT,
                                format!(
                                    "tag {tag} was baked on {} (fingerprint {t}) and \
                                     cannot be restored on {asked} (fingerprint {n}): \
                                     a runsc checkpoint is not portable across \
                                     differing hosts",
                                    snap.node_id
                                ),
                            ));
                        }
                    }
                    return Err(err(
                        StatusCode::CONFLICT,
                        format!(
                            "tag {tag} is on node {} and its image is node-local, \
                             so it cannot be forked on {asked}. Drop node_id to \
                             fork it where it lives.",
                            snap.node_id
                        ),
                    ));
                }
            }
            (snap.template, None, Some(snap.node_id))
        }
        None => {
            if store
                .get_template(&template)
                .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
                .is_none()
            {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    format!("unknown template: {template}"),
                ));
            }
            (template, req.pool.clone(), req.node_id.clone())
        }
    };

    let pool = pool.unwrap_or_else(|| "default".into());
    let node_id = match node_id {
        Some(n) => n,
        None => match pick_node(&st, &store, &pool) {
            Ok(n) => n,
            Err(e) => {
                // Counted here rather than from the status code: a 503 out
                // of the scheduler means the fleet had no room, which is a
                // different thing from the master being broken.
                st.metrics.creates_refused.fetch_add(1, Relaxed);
                return Err(e);
            }
        },
    };

    let mut made = Vec::with_capacity(count as usize);
    for _ in 0..count {
        // The port is the sandbox's own, inside its namespace, so there is
        // nothing to allocate -- two sandboxes may both listen on 8080.
        let port = Some(req.port.unwrap_or(DEFAULT_SANDBOX_PORT));
        // A host port for the gateway, distinct from `port` above, which is
        // inside the namespace and is 8080 for nearly every sandbox.
        //
        // This revives `allocate_port`, which was written in #324 when ports
        // had to be unique per node, made redundant by per-sandbox
        // namespaces in #328, and then sat unused -- called only by its own
        // tests, with `--port-min`/`--port-max` still on the CLI. The
        // gateway needs exactly what it does, so it is wired to the field it
        // was always right for.
        let gateway_port = store
            .allocate_gateway_port(&node_id, st.port_range.0, st.port_range.1)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        if gateway_port.is_none() {
            tracing::warn!(
                node = %node_id,
                range = ?st.port_range,
                "no free gateway port; this sandbox is reachable only from its node"
            );
        }

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
                .clone()
                // 128 bits, not the 48 that `[..12]` gave. A sandbox id is
                // handed to callers and appears in URLs, and RFC 16.5 asks
                // for an id that cannot be guessed or enumerated -- 48 bits
                // is inside reach of an attacker who can make requests.
                .unwrap_or_else(|| format!("sb-{}", random_token())),
            node_id: node_id.clone(),
            pool: pool.clone(),
            template: template.clone(),
            tier: Tier::Hot,
            token: random_token(),
            volume: req.volume.clone(),
            port,
            created_at: t,
            last_active_at: t,
            last_error: None,
            error_attempts: 0,
            net_index,
            ip: None,
            from_tag: from_tag.clone(),
            memory_limit_mib: req.memory_limit_mib,
            gateway_port,
        };
        store
            .insert_sandbox(&sb)
            .map_err(|e| err(StatusCode::CONFLICT, e.to_string()))?;
        let token = sb.token.clone();
        let mut v = serde_json::to_value(SandboxView::from(sb))
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        v["token"] = serde_json::Value::String(token);
        made.push(v);
    }

    st.metrics
        .creates_placed
        .fetch_add(made.len() as u64, Relaxed);

    // The node is parked on its stream; this reaches it now.
    st.wake_node(&node_id);

    // v0.5 callers send `n` and expect a list; a v0.6 caller creating one
    // gets the object it asked for.
    Ok(if req.n.is_some() {
        (StatusCode::CREATED, Json(serde_json::Value::Array(made)))
    } else {
        (
            StatusCode::CREATED,
            Json(made.into_iter().next().expect("count >= 1")),
        )
    })
}

/// Least-loaded admitting node in a pool.
fn pick_node(st: &St, store: &Store, pool: &str) -> Result<String, ApiErr> {
    let nodes = store
        .list_nodes()
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let mut best: Option<(String, u32)> = None;
    let mut refused: Vec<String> = Vec::new();
    for n in nodes.into_iter().filter(|n| n.connected && n.pool == pool) {
        let load = store
            .count_sandboxes_on_node(&n.id)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        if let Some(why) = st.admit.refuse(&n, load) {
            refused.push(format!("{}: {why}", n.id));
            continue;
        }
        if best.as_ref().is_none_or(|(_, b)| load < *b) {
            best = Some((n.id, load));
        }
    }
    match best {
        Some((id, _)) => Ok(id),
        None if !refused.is_empty() => Err(err(
            StatusCode::SERVICE_UNAVAILABLE,
            format!(
                "every node in pool {pool} is too loaded: {}",
                refused.join("; ")
            ),
        )),
        None => Err(err(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("no connected node in pool {pool}"),
        )),
    }
}

async fn list_sandboxes(State(st): State<St>) -> Result<Json<Vec<SandboxView>>, ApiErr> {
    let all = st
        .store()
        .list_sandboxes()
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(all.into_iter().map(Into::into).collect()))
}

async fn get_sandbox(
    State(st): State<St>,
    Path(id): Path<String>,
) -> Result<Json<SandboxView>, ApiErr> {
    let got = st
        .store()
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
    let store = st.store();
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
        let store = st.store();
        let node_id = store
            .get_sandbox(&id)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            .map(|s| s.node_id);
        store
            .delete_sandbox(&id)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        node_id
    };
    st.metrics.deletes.fetch_add(1, Relaxed);
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
    st.store()
        .put_template(&req.name, &req.rootfs, &req.spec.to_string())
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(StatusCode::CREATED)
}

async fn list_templates(State(st): State<St>) -> Result<Json<Vec<String>>, ApiErr> {
    let names = st
        .store()
        .list_templates()
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(names))
}
