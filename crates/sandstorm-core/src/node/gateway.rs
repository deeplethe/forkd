//! The sandbox gateway: what a sandbox's access token actually guards.
//!
//! RFC 16.5 gives every sandbox a 128-bit token and says "访问它的任何端口都
//! 要带" -- the token is required on any port of that sandbox. Until now
//! there was nothing to require it on: a sandbox listens inside its own
//! namespace and the node's rules drop traffic from siblings and from
//! off-box, so it was reachable only from its own node and the token had
//! nothing to gate. This is the piece that exposes a port and makes the
//! token load-bearing.
//!
//! **Why the gateway is on the node.** Three parts of the RFC pull in
//! different directions and only this placement satisfies all three:
//!
//! - §8 wants port exposure to be "每沙箱一条 DNAT", a plain L4 rule.
//! - §16.5 wants the token checked -- "网关校验" -- which L4 cannot do,
//!   because a DNAT has nowhere to read a credential.
//! - §15.5 says the data plane is "网关 → 节点(直连)" and that a wake
//!   request must never travel through the master.
//!
//! So the gateway cannot be a DNAT (no place for the token) and cannot be
//! on the master (§15.5). It runs on the node, in front of the sandbox.
//!
//! **What that costs: the exposed port is HTTP in v0.6.** A raw TCP stream
//! has nowhere to carry a token, so a protocol-agnostic gateway could not
//! check one -- it would be a DNAT again. HTTP is what the templates serve,
//! and the header is the same `x-sandstorm-sandbox-token` the API already uses.
//! Exposing a non-HTTP port needs a different credential mechanism and is
//! not built.
//!
//! **Cold sandboxes need no tier lookup here.** #327 made a sandbox that is
//! not hot REJECT connections with a TCP reset, so the forward fails at once
//! with connection-refused rather than hanging. The gateway takes that as
//! the sandbox being cold, wakes it (`wake.rs`) and forwards again, so the
//! caller sees a slower response rather than an error. It therefore holds
//! no tier state and cannot go stale against it; a listener stays up for
//! every tier that is on this node, frozen and snapshotted included.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use tokio::task::JoinHandle;

use super::wake::Wakeups;
use crate::master::SANDBOX_TOKEN_HEADER;

/// How the response says a request had to wait for a wake, and for how
/// long. For whoever is measuring cold starts; nothing depends on it.
pub const WAKE_HEADER: &str = "x-sandstorm-woke-ms";

/// How long a request waits for a sandbox that is up but not yet listening.
const BOOT_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// Everything the gateway needs to serve one sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exposed {
    pub id: String,
    /// Host port the gateway listens on. Allocated per node by the master,
    /// so two sandboxes on one machine never collide.
    pub gateway_port: u16,
    /// The sandbox's address inside its namespace, and the port its process
    /// listens on there -- which may be 8080 for every sandbox, since each
    /// has a namespace of its own.
    pub sandbox_ip: Ipv4Addr,
    pub sandbox_port: u16,
    /// The sandbox's own access token. Travels from the master over the
    /// mTLS control channel and is kept in the bundle so a node restart
    /// does not leave the port open without a credential to check.
    pub token: String,
}

/// What a listener is bound to. A change here needs a new socket; a change
/// to the token alone does not.
#[derive(PartialEq, Eq)]
struct Binding {
    gateway_port: u16,
    sandbox_ip: Ipv4Addr,
    sandbox_port: u16,
}

impl From<&Exposed> for Binding {
    fn from(e: &Exposed) -> Self {
        Self {
            gateway_port: e.gateway_port,
            sandbox_ip: e.sandbox_ip,
            sandbox_port: e.sandbox_port,
        }
    }
}

struct Target {
    id: String,
    upstream: String,
    /// Updated in place when the sandbox is re-registered with a new token.
    ///
    /// Restarting the listener instead meant rebinding the same port right
    /// after aborting the task that held it, and under a parallel test suite
    /// that lost often enough to be flaky -- while leaving the sandbox with
    /// no gateway at all when it lost in production. A token change no
    /// longer touches the socket; only a port or address change does.
    token: std::sync::RwLock<String>,
    client: reqwest::Client,
    /// `None` only in tests that exercise the token check alone.
    wake: Option<Arc<Wakeups>>,
}

/// Listeners for the sandboxes currently exposed on this node.
struct Live {
    exposed: Exposed,
    target: Arc<Target>,
    handle: JoinHandle<()>,
}

#[derive(Default)]
pub struct Gateway {
    running: HashMap<String, Live>,
    wake: Option<Arc<Wakeups>>,
}

impl Gateway {
    /// A gateway that answers a cold sandbox's requests with a 503.
    pub fn new() -> Self {
        Self::default()
    }

    /// A gateway that wakes a cold sandbox when a request arrives for it.
    pub fn waking(wake: Arc<Wakeups>) -> Self {
        Self {
            running: HashMap::new(),
            wake: Some(wake),
        }
    }

    /// Bring the listeners in line with what should be exposed.
    ///
    /// Reconciled from the observed sandboxes after every poll rather than
    /// driven by create and destroy events, for the same reason the rest of
    /// the node works that way: a listener that leaked past its sandbox
    /// would leave a port open on the host, and a missed create would leave
    /// a sandbox unreachable with nothing to say why.
    pub async fn reconcile(&mut self, want: &[Exposed]) {
        let wanted: HashMap<&str, &Exposed> = want.iter().map(|e| (e.id.as_str(), e)).collect();

        // Gone, or bound to something else: only these need the socket
        // closed and reopened.
        let mut stale: Vec<String> = Vec::new();
        for (id, live) in self.running.iter() {
            let rebind = match wanted.get(id.as_str()) {
                None => true,
                Some(w) => Binding::from(*w) != Binding::from(&live.exposed),
            };
            if live.handle.is_finished() || rebind {
                stale.push(id.clone());
            }
        }
        for id in stale {
            if let Some(live) = self.running.remove(&id) {
                live.handle.abort();
                // Awaited so the port is released before anything rebinds
                // it. `abort` only requests cancellation.
                let _ = live.handle.await;
                tracing::info!(id = %id, port = live.exposed.gateway_port, "gateway stopped");
            }
        }

        for e in want {
            if let Some(live) = self.running.get_mut(&e.id) {
                // Same socket, possibly a new credential: swap it in place.
                if live.exposed.token != e.token {
                    if let Ok(mut t) = live.target.token.write() {
                        *t = e.token.clone();
                    }
                    live.exposed = e.clone();
                    tracing::info!(id = %e.id, "gateway token rotated");
                }
                continue;
            }
            match spawn_one(e.clone(), self.wake.clone()).await {
                Ok((target, handle)) => {
                    tracing::info!(
                        id = %e.id, port = e.gateway_port,
                        upstream = %format!("{}:{}", e.sandbox_ip, e.sandbox_port),
                        "gateway listening"
                    );
                    self.running.insert(
                        e.id.clone(),
                        Live {
                            exposed: e.clone(),
                            target,
                            handle,
                        },
                    );
                }
                Err(err) => tracing::warn!(
                    id = %e.id, port = e.gateway_port, error = %err,
                    "gateway could not bind; the sandbox stays unreachable from off-box"
                ),
            }
        }
    }

    pub fn exposed_count(&self) -> usize {
        self.running.len()
    }
}

async fn spawn_one(
    e: Exposed,
    wake: Option<Arc<Wakeups>>,
) -> anyhow::Result<(Arc<Target>, JoinHandle<()>)> {
    let bind: SocketAddr = format!("0.0.0.0:{}", e.gateway_port).parse()?;
    let listener = tokio::net::TcpListener::bind(bind).await?;
    let target = Arc::new(Target {
        id: e.id.clone(),
        upstream: format!("http://{}:{}", e.sandbox_ip, e.sandbox_port),
        token: std::sync::RwLock::new(e.token.clone()),
        // No connect pooling across sandboxes: one client per listener, so
        // a connection can never be reused against a different sandbox.
        client: reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()?,
        wake,
    });
    let app = axum::Router::new()
        .fallback(proxy)
        .with_state(target.clone());
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok((target, handle))
}

/// Check the token, then pass the request through.
async fn proxy(State(t): State<Arc<Target>>, req: Request) -> Response {
    let expected = t.token.read().map(|g| g.clone()).unwrap_or_default();
    if !presented_token_matches(req.headers(), &expected) {
        // 401, not 404: the port is open and answering, and pretending
        // otherwise would make a missing header look like a wrong address.
        return (
            StatusCode::UNAUTHORIZED,
            [("content-type", "application/json")],
            format!(
                r#"{{"error":"this sandbox requires its own access token in the {SANDBOX_TOKEN_HEADER} header; it is the `token` field of the response that created the sandbox"}}"#
            ),
        )
            .into_response();
    }

    let (parts, body) = req.into_parts();
    let path = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");
    let bytes = match axum::body::to_bytes(body, 32 * 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("unreadable body: {e}")).into_response()
        }
    };

    let mut headers = parts.headers.clone();
    // Not forwarded: it authenticates the caller to the gateway and has no
    // meaning to the sandbox, which should never see a credential it could
    // log or leak.
    headers.remove(SANDBOX_TOKEN_HEADER);
    headers.remove(axum::http::header::HOST);

    let url = format!("{}{}", t.upstream, path);
    let send = || {
        t.client
            .request(parts.method.clone(), &url)
            .headers(headers.clone())
            .body(bytes.clone())
            .send()
    };

    // Before forwarding, so a demotion that arrives while this request is
    // in flight is deferred rather than pulling the sandbox out from under
    // it.
    if let Some(w) = &t.wake {
        w.saw(&t.id);
    }
    let mut sent = send().await;
    let mut woke = None;
    if let (Err(e), Some(w)) = (&sent, &t.wake) {
        if e.is_connect() {
            match w.wake(&t.id).await {
                Ok(took) => {
                    tracing::info!(id = %t.id, ms = took.as_millis() as u64, "woke on traffic");
                    woke = Some(took);
                    sent = send().await;
                    // Up and still refusing: the program is still starting
                    // -- a sandbox created a moment ago, typically. Wait for
                    // it a while rather than answer 502 to a request that
                    // would have succeeded a second later.
                    let deadline = std::time::Instant::now() + BOOT_WAIT;
                    let mut pause = std::time::Duration::from_millis(50);
                    while sent.as_ref().is_err_and(|e| e.is_connect())
                        && std::time::Instant::now() < deadline
                    {
                        tokio::time::sleep(pause).await;
                        pause = (pause * 2).min(std::time::Duration::from_millis(500));
                        sent = send().await;
                    }
                }
                Err(e) => {
                    tracing::warn!(id = %t.id, error = %format!("{e:#}"), "could not wake on traffic");
                    return (
                        StatusCode::SERVICE_UNAVAILABLE,
                        [("content-type", "application/json"), ("retry-after", "5")],
                        serde_json::json!({
                            "error": format!("sandbox {} is cold and could not be woken: {e:#}", t.id)
                        })
                        .to_string(),
                    )
                        .into_response();
                }
            }
        }
    }

    match sent {
        Ok(r) => {
            let status = r.status();
            let hdrs = r.headers().clone();
            match r.bytes().await {
                Ok(b) => {
                    let mut out = Response::builder().status(status);
                    for (k, v) in hdrs.iter() {
                        out = out.header(k, v);
                    }
                    if let Some(took) = woke {
                        out = out.header(WAKE_HEADER, took.as_millis().to_string());
                    }
                    out.body(Body::from(b))
                        .unwrap_or_else(|e| {
                            (StatusCode::BAD_GATEWAY, format!("{e}")).into_response()
                        })
                }
                Err(e) => (StatusCode::BAD_GATEWAY, format!("upstream body: {e}")).into_response(),
            }
        }
        // A sandbox that is not hot REJECTs with a reset (#327), so this is
        // the ordinary cold case and arrives in milliseconds rather than as
        // a timeout. A gateway that wakes has already tried that and only
        // gets here if the sandbox came up and still refused. Said plainly,
        // because a bare 502 would look like the sandbox had crashed.
        Err(e) if e.is_connect() && woke.is_some() => (
            StatusCode::BAD_GATEWAY,
            [("content-type", "application/json")],
            format!(
                r#"{{"error":"sandbox {} was woken but nothing is listening on port {} inside it"}}"#,
                t.id,
                t.upstream.rsplit(':').next().unwrap_or("?")
            ),
        )
            .into_response(),
        Err(e) if e.is_connect() => (
            StatusCode::SERVICE_UNAVAILABLE,
            [("content-type", "application/json")],
            format!(
                r#"{{"error":"sandbox {} is not serving: it is frozen, snapshotted or detached. Promote it with POST /v1/sandboxes/{}/touch and retry."}}"#,
                t.id, t.id
            ),
        )
            .into_response(),
        Err(e) if e.is_timeout() => (
            StatusCode::GATEWAY_TIMEOUT,
            format!("sandbox {} did not answer within 60s", t.id),
        )
            .into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, format!("{e}")).into_response(),
    }
}

fn presented_token_matches(headers: &HeaderMap, expected: &str) -> bool {
    let presented = headers
        .get(SANDBOX_TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    crate::auth::secrets_match(presented, expected)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(id: &str, port: u16) -> Exposed {
        Exposed {
            id: id.into(),
            gateway_port: port,
            sandbox_ip: "10.86.0.2".parse().unwrap(),
            sandbox_port: 8080,
            token: "a".repeat(32),
        }
    }

    #[test]
    fn a_missing_header_does_not_match() {
        let h = HeaderMap::new();
        assert!(!presented_token_matches(&h, &"a".repeat(32)));
    }

    #[test]
    fn the_wrong_token_does_not_match() {
        let mut h = HeaderMap::new();
        h.insert(SANDBOX_TOKEN_HEADER, "b".repeat(32).parse().unwrap());
        assert!(!presented_token_matches(&h, &"a".repeat(32)));
        // A prefix must not pass either -- the comparison is padded, so a
        // short token cannot match on its first bytes.
        h.insert(SANDBOX_TOKEN_HEADER, "a".repeat(16).parse().unwrap());
        assert!(!presented_token_matches(&h, &"a".repeat(32)));
    }

    #[test]
    fn the_right_token_matches() {
        let mut h = HeaderMap::new();
        h.insert(SANDBOX_TOKEN_HEADER, "a".repeat(32).parse().unwrap());
        assert!(presented_token_matches(&h, &"a".repeat(32)));
    }

    /// Two ports below the ephemeral range.
    ///
    /// Three wrong versions came first and the reasons are worth keeping,
    /// because each looked right:
    ///
    /// 1. Hard-coded 38011/38012. Linux hands out ephemeral ports from
    ///    32768, so a parallel test binding `:0` can be given one of them.
    /// 2. Asking the kernel with `:0` and releasing it. Worse: that returns
    ///    a port inside the ephemeral range by definition, and the observed
    ///    failure was `port=57621 Address already in use` after something
    ///    took it between the release and the rebind.
    /// 3. Fixed ports under 32768, which fixed the port choice but not the
    ///    flake -- because the flake was never about port choice. It was the
    ///    rebind itself, and the fix was to stop rebinding when only the
    ///    token changes.
    ///
    /// These stay under 32768 so `:0` never collides with them, on Linux or
    /// macOS. Do not "simplify" them back to `:0`. The wake tests use
    /// 28461-28464 for the same reason.
    async fn free_pair() -> (u16, u16) {
        let a = tokio::net::TcpListener::bind(("127.0.0.1", 28451u16))
            .await
            .expect("28451 free");
        let b = tokio::net::TcpListener::bind(("127.0.0.1", 28452u16))
            .await
            .expect("28452 free");
        let (pa, pb) = (
            a.local_addr().unwrap().port(),
            b.local_addr().unwrap().port(),
        );
        drop(a);
        drop(b);
        (pa, pb)
    }

    /// A plain HTTP server on `port` that answers every request with "hi".
    /// What a sandbox looks like once it is woken.
    fn serve_hi(port: u16) {
        use std::io::{Read, Write};
        let l = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
        std::thread::spawn(move || {
            for mut s in l.incoming().flatten() {
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf);
                let _ = s.write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nhi",
                );
            }
        });
    }

    async fn get(port: u16) -> reqwest::Response {
        reqwest::Client::new()
            .get(format!("http://127.0.0.1:{port}/x"))
            .header(SANDBOX_TOKEN_HEADER, "a".repeat(32))
            .send()
            .await
            .unwrap()
    }

    /// The whole point: a request for a cold sandbox wakes it and gets the
    /// sandbox's answer, not a 503 telling the caller to go and wake it.
    #[tokio::test]
    async fn a_request_for_a_cold_sandbox_wakes_it() {
        let (gw_port, upstream) = (28461u16, 28462u16);
        let woken = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let w = {
            let woken = woken.clone();
            Arc::new(Wakeups::new(move |_| {
                // Nothing listens until the first wake, which is how a
                // frozen sandbox behind #327's REJECT looks from here.
                if woken.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                    serve_hi(upstream);
                }
                Ok(())
            }))
        };
        let mut g = Gateway::waking(w.clone());
        let mut e = spec("sb-cold", gw_port);
        e.sandbox_ip = "127.0.0.1".parse().unwrap();
        e.sandbox_port = upstream;
        g.reconcile(&[e]).await;

        let r = get(gw_port).await;
        assert_eq!(r.status(), 200);
        assert!(
            r.headers().contains_key(WAKE_HEADER),
            "a woken request says so"
        );
        assert_eq!(r.text().await.unwrap(), "hi");
        assert_eq!(woken.load(std::sync::atomic::Ordering::SeqCst), 1);

        // Now it is up: no second wake, and nothing says there was one.
        let r = get(gw_port).await;
        assert_eq!(r.status(), 200);
        assert!(!r.headers().contains_key(WAKE_HEADER));
        assert_eq!(woken.load(std::sync::atomic::Ordering::SeqCst), 1);

        // Both requests are reported to the master, once.
        assert_eq!(w.take_traffic(), vec!["sb-cold".to_string()]);
        g.reconcile(&[]).await;
    }

    #[tokio::test]
    async fn a_wake_the_node_refuses_is_a_503_with_the_reason() {
        let gw_port = 28463u16;
        let w = Arc::new(Wakeups::new(|_| {
            anyhow::bail!("node held it back: 100 MiB free, below its own 512 MiB floor")
        }));
        let mut g = Gateway::waking(w);
        let mut e = spec("sb-full", gw_port);
        e.sandbox_ip = "127.0.0.1".parse().unwrap();
        // Nothing will ever listen here.
        e.sandbox_port = 28464;
        g.reconcile(&[e]).await;

        let r = get(gw_port).await;
        assert_eq!(r.status(), 503);
        assert!(r.headers().contains_key("retry-after"));
        let body = r.text().await.unwrap();
        assert!(body.contains("512 MiB floor"), "{body}");
        g.reconcile(&[]).await;
    }

    #[tokio::test]
    async fn reconcile_starts_stops_and_replaces() {
        let mut g = Gateway::new();
        let (pa, pb) = free_pair().await;
        let a = spec("sb-a", pa);
        let b = spec("sb-b", pb);
        assert_ne!(a.gateway_port, b.gateway_port);

        g.reconcile(&[a.clone(), b.clone()]).await;
        assert_eq!(g.exposed_count(), 2);

        // Dropping one stops only that listener.
        g.reconcile(std::slice::from_ref(&a)).await;
        assert_eq!(g.exposed_count(), 1);

        // A changed token must replace the listener rather than be ignored:
        // otherwise a re-registered sandbox would keep accepting the old
        // credential.
        let mut a2 = a.clone();
        a2.token = "c".repeat(32);
        g.reconcile(&[a2.clone()]).await;
        assert_eq!(
            g.exposed_count(),
            1,
            "replacing a listener rebinds the same port, which races the \
             abort unless it is awaited"
        );
        assert_eq!(
            g.running.get("sb-a").map(|l| l.exposed.token.clone()),
            Some(a2.token)
        );

        g.reconcile(&[]).await;
        assert_eq!(g.exposed_count(), 0);
    }
}
