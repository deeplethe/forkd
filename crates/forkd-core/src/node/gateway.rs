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
//! and the header is the same `x-forkd-sandbox-token` the API already uses.
//! Exposing a non-HTTP port needs a different credential mechanism and is
//! not built.
//!
//! **Cold sandboxes need no tier lookup here.** #327 made a sandbox that is
//! not hot REJECT connections with a TCP reset, so the forward fails at once
//! with connection-refused rather than hanging, and the gateway turns that
//! into a 503 naming the reason. The gateway therefore holds no tier state
//! and cannot go stale against it.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use tokio::task::JoinHandle;

use crate::master::SANDBOX_TOKEN_HEADER;

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

struct Target {
    id: String,
    upstream: String,
    token: String,
    client: reqwest::Client,
}

/// Listeners for the sandboxes currently exposed on this node.
#[derive(Default)]
pub struct Gateway {
    running: HashMap<String, (Exposed, JoinHandle<()>)>,
}

impl Gateway {
    pub fn new() -> Self {
        Self::default()
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

        // Gone, or changed in a way the listener cannot follow (a different
        // port or a re-registered token): stop it.
        // Written out rather than chained: `iter().filter(|(id, (have, h))|
        // ...)` hands the closure a reference to a tuple of references, and
        // the resulting `have` compared unequal to an identical value, so
        // every listener looked stale and was torn down on the next pass.
        let mut stale: Vec<String> = Vec::new();
        for (id, (have, handle)) in self.running.iter() {
            let finished = handle.is_finished();
            let changed = match wanted.get(id.as_str()) {
                None => true,
                Some(w) => *w != have,
            };
            if finished || changed {
                stale.push(id.clone());
            }
        }
        for id in stale {
            if let Some((e, h)) = self.running.remove(&id) {
                h.abort();
                // Awaited, not just aborted. `abort` only requests it: the
                // task still holds the listener until it is actually
                // dropped, so rebinding the same port immediately -- which
                // is exactly what replacing a listener does -- raced and
                // failed with address-in-use. The sandbox then silently
                // had no gateway.
                let _ = h.await;
                tracing::info!(id = %id, port = e.gateway_port, "gateway stopped");
            }
        }

        for e in want {
            if self.running.contains_key(&e.id) {
                continue;
            }
            match spawn_one(e.clone()).await {
                Ok(h) => {
                    tracing::info!(
                        id = %e.id, port = e.gateway_port,
                        upstream = %format!("{}:{}", e.sandbox_ip, e.sandbox_port),
                        "gateway listening"
                    );
                    self.running.insert(e.id.clone(), (e.clone(), h));
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

async fn spawn_one(e: Exposed) -> anyhow::Result<JoinHandle<()>> {
    let bind: SocketAddr = format!("0.0.0.0:{}", e.gateway_port).parse()?;
    let listener = tokio::net::TcpListener::bind(bind).await?;
    let target = Arc::new(Target {
        id: e.id.clone(),
        upstream: format!("http://{}:{}", e.sandbox_ip, e.sandbox_port),
        token: e.token.clone(),
        // No connect pooling across sandboxes: one client per listener, so
        // a connection can never be reused against a different sandbox.
        client: reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()?,
    });
    let app = axum::Router::new().fallback(proxy).with_state(target);
    Ok(tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    }))
}

/// Check the token, then pass the request through.
async fn proxy(State(t): State<Arc<Target>>, req: Request) -> Response {
    if !presented_token_matches(req.headers(), &t.token) {
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
    let sent = t
        .client
        .request(parts.method.clone(), &url)
        .headers(headers)
        .body(bytes)
        .send()
        .await;

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
        // a timeout. Said plainly, because a bare 502 would look like the
        // sandbox had crashed.
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

    #[tokio::test]
    async fn reconcile_starts_stops_and_replaces() {
        let mut g = Gateway::new();
        // Port 0 would be chosen by the kernel, which defeats the point, so
        // these use high fixed ports; binding is the only side effect.
        let a = spec("sb-a", 38011);
        let b = spec("sb-b", 38012);

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
            g.running.get("sb-a").map(|(e, _)| e.token.clone()),
            Some(a2.token)
        );

        g.reconcile(&[]).await;
        assert_eq!(g.exposed_count(), 0);
    }
}
