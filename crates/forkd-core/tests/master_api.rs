//! End-to-end through the master's HTTP surface: register a node, place a
//! sandbox, and check the node is told to create it. No runsc involved --
//! this is the control plane on its own.

use std::net::SocketAddr;
use std::sync::Arc;

use forkd_core::identity::Ca;
use forkd_core::master::{router, MasterState};
use forkd_core::proto::{ObservedSandbox, PollResponse};
use forkd_core::state::Store;
use forkd_core::tier::Tier;
use tempfile::TempDir;

struct Harness {
    base: String,
    _td: TempDir,
}

async fn start() -> Harness {
    let td = TempDir::new().unwrap();
    let st = Arc::new(MasterState::new(
        Store::open(td.path().join("state.db")).unwrap(),
        Ca::open_or_create(td.path().join("pki")).unwrap(),
        forkd_core::master::MasterOptions {
            port_range: (20000, 20010),
            // Admission off and the API open in tests: a node that has not
            // polled reports zero free memory, and most tests never poll.
            admit: forkd_core::master::Admission {
                min_free_mib: 0,
                max_psi_avg10: f64::MAX,
            },
            ..Default::default()
        },
    ));
    let listener = tokio::net::TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router(st)).await.ok();
    });
    Harness {
        base: format!("http://{addr}"),
        _td: td,
    }
}

async fn register(h: &Harness, node_id: &str, pool: &str) -> serde_json::Value {
    let c = reqwest::Client::new();
    let tok: serde_json::Value = c
        .post(format!("{}/v1/node-tokens", h.base))
        .json(&serde_json::json!({ "pool": pool }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    c.post(format!("{}/v1/node/register", h.base))
        .json(&serde_json::json!({
            "token": tok["token"],
            "node_id": node_id,
            "fingerprint": "abc123",
            "max_sandboxes": 100,
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn put_template(h: &Harness, name: &str) {
    let r = reqwest::Client::new()
        .post(format!("{}/v1/templates", h.base))
        .json(&serde_json::json!({
            "name": name,
            "rootfs": "rootfs",
            "spec": {
                "args": ["/usr/bin/python3", "/app/server.py", "{{PORT}}"],
                "memory_max_mib": 256
            }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
}

#[tokio::test]
async fn node_registers_and_gets_an_mtls_identity() {
    let h = start().await;
    let r = register(&h, "bm-1", "default").await;
    assert_eq!(r["node_id"], "bm-1");
    assert_eq!(r["pool"], "default");
    // Both halves of mTLS: the node's own cert, and the CA it uses to
    // verify the master is the real master.
    assert!(r["client_cert"]
        .as_str()
        .unwrap()
        .contains("BEGIN CERTIFICATE"));
    assert!(r["client_key"].as_str().unwrap().contains("PRIVATE KEY"));
    assert!(r["ca_cert"].as_str().unwrap().contains("BEGIN CERTIFICATE"));
}

#[tokio::test]
async fn a_registration_token_is_worth_one_node() {
    let h = start().await;
    let c = reqwest::Client::new();
    let tok: serde_json::Value = c
        .post(format!("{}/v1/node-tokens", h.base))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let body = serde_json::json!({
        "token": tok["token"], "node_id": "first",
        "fingerprint": "f", "max_sandboxes": 1,
    });
    let first = c
        .post(format!("{}/v1/node/register", h.base))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), 200);

    let mut again = body.clone();
    again["node_id"] = "second".into();
    let second = c
        .post(format!("{}/v1/node/register", h.base))
        .json(&again)
        .send()
        .await
        .unwrap();
    assert_eq!(
        second.status(),
        401,
        "a used token must not register a second node"
    );
}

#[tokio::test]
async fn poll_tells_the_node_to_create_what_it_does_not_have() {
    let h = start().await;
    register(&h, "bm-1", "default").await;
    put_template(&h, "python").await;
    let c = reqwest::Client::new();

    let created: serde_json::Value = c
        .post(format!("{}/v1/sandboxes", h.base))
        .json(&serde_json::json!({ "template": "python", "port": 19800 }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();
    assert_eq!(
        created["node_id"], "bm-1",
        "placed on the only node in the pool"
    );
    // The sandbox access token is shown once, at creation, and never again.
    assert_eq!(created["token"].as_str().unwrap().len(), 32);

    let resp: PollResponse = c
        .post(format!("{}/v1/node/poll", h.base))
        .json(&serde_json::json!({
            "node_id": "bm-1", "observed": [],
            "free_memory_mib": 2000, "pressure_some_avg10": 0.0, "hold_ms": 0,
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp.commands.len(), 1, "one create");
    match &resp.commands[0] {
        forkd_core::proto::Command::Create {
            id: got,
            port,
            spec,
            ..
        } => {
            assert_eq!(got, &id);
            assert_eq!(*port, 19800);
            // The spec travels with the command, so a node needs only the
            // rootfs locally.
            assert_eq!(spec["memory_max_mib"], 256);
            assert_eq!(spec["args"][1], "/app/server.py");
        }
        other => panic!("expected Create, got {other:?}"),
    }

    // Reading a sandbox back must never hand out its token again.
    let fetched: serde_json::Value = c
        .get(format!("{}/v1/sandboxes/{id}", h.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        fetched.get("token").is_none(),
        "token must not be re-served"
    );
}

/// The property that makes a restart free: a node reporting a sandbox the
/// master already wants is told nothing, not told to recreate it.
#[tokio::test]
async fn re_adoption_issues_no_commands() {
    let h = start().await;
    register(&h, "bm-1", "default").await;
    put_template(&h, "python").await;
    let c = reqwest::Client::new();
    let created: serde_json::Value = c
        .post(format!("{}/v1/sandboxes", h.base))
        .json(&serde_json::json!({ "template": "python", "port": 19801 }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();

    let resp: PollResponse = c
        .post(format!("{}/v1/node/poll", h.base))
        .json(&serde_json::json!({
            "node_id": "bm-1",
            "observed": [ObservedSandbox { id: id.clone(), tier: Tier::Hot, serving: true, ip: None, net_index: None }],
            "free_memory_mib": 2000, "pressure_some_avg10": 0.0, "hold_ms": 0,
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        resp.commands.is_empty(),
        "an already-running sandbox must be adopted, not recreated: {:?}",
        resp.commands
    );
}

/// A node holding something desired state has dropped is told to destroy it
/// -- and only then. This is the one path that may kill a sandbox.
#[tokio::test]
async fn deleting_a_sandbox_makes_the_node_destroy_it() {
    let h = start().await;
    register(&h, "bm-1", "default").await;
    put_template(&h, "python").await;
    let c = reqwest::Client::new();
    let created: serde_json::Value = c
        .post(format!("{}/v1/sandboxes", h.base))
        .json(&serde_json::json!({ "template": "python" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();

    let del = c
        .delete(format!("{}/v1/sandboxes/{id}", h.base))
        .send()
        .await
        .unwrap();
    assert_eq!(del.status(), 204);

    let resp: PollResponse = c
        .post(format!("{}/v1/node/poll", h.base))
        .json(&serde_json::json!({
            "node_id": "bm-1",
            "observed": [ObservedSandbox { id: id.clone(), tier: Tier::Hot, serving: true, ip: None, net_index: None }],
            "free_memory_mib": 2000, "pressure_some_avg10": 0.0, "hold_ms": 0,
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        matches!(
            resp.commands.as_slice(),
            [forkd_core::proto::Command::Destroy { id: got, .. }] if got == &id
        ),
        "got {:?}",
        resp.commands
    );
}

#[tokio::test]
async fn touching_a_cold_sandbox_promotes_it_straight_to_hot() {
    let h = start().await;
    register(&h, "bm-1", "default").await;
    put_template(&h, "python").await;
    let c = reqwest::Client::new();
    let created: serde_json::Value = c
        .post(format!("{}/v1/sandboxes", h.base))
        .json(&serde_json::json!({ "template": "python" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();

    // Pretend the demoter has already put it on disk.
    let poll_then = |tier: Tier| {
        let base = h.base.clone();
        let id = id.clone();
        async move {
            reqwest::Client::new()
                .post(format!("{base}/v1/node/poll"))
                .json(&serde_json::json!({
                    "node_id": "bm-1",
                    "observed": [ObservedSandbox { id, tier, serving: false, ip: None, net_index: None }],
                    "free_memory_mib": 2000, "pressure_some_avg10": 0.0, "hold_ms": 0,
                }))
                .send()
                .await
                .unwrap()
                .json::<PollResponse>()
                .await
                .unwrap()
        }
    };

    let touched: serde_json::Value = c
        .post(format!("{}/v1/sandboxes/{id}/touch", h.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(touched["tier"], "hot");

    // A node still holding it as a snapshot is told to bring it up.
    let resp = poll_then(Tier::Snapshot).await;
    assert!(
        matches!(
            resp.commands.as_slice(),
            [forkd_core::proto::Command::SetTier {
                tier: Tier::Hot,
                ..
            }]
        ),
        "got {:?}",
        resp.commands
    );
}

#[tokio::test]
async fn a_sandbox_cannot_be_placed_outside_its_pool() {
    let h = start().await;
    register(&h, "bj-1", "beijing").await;
    put_template(&h, "python").await;

    let r = reqwest::Client::new()
        .post(format!("{}/v1/sandboxes", h.base))
        .json(&serde_json::json!({ "template": "python", "pool": "hongkong" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        503,
        "no node in that pool, and pools must not be crossed"
    );
}

#[tokio::test]
async fn an_unknown_template_is_refused() {
    let h = start().await;
    register(&h, "bm-1", "default").await;
    let r = reqwest::Client::new()
        .post(format!("{}/v1/sandboxes", h.base))
        .json(&serde_json::json!({ "template": "no-such-thing" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
}

/// "Add a machine" in the panel produces a command, not a machine. The
/// master has no way to reach a host that has never dialled in, so the
/// join step necessarily runs over there.
#[tokio::test]
async fn bootstrap_returns_a_pastable_join_command() {
    let h = start().await;
    let v: serde_json::Value = reqwest::Client::new()
        .post(format!("{}/v1/node-bootstrap", h.base))
        .json(&serde_json::json!({
            "pool": "beijing",
            "master_url": "https://forkd.example.com"
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let cmd = v["install_command"].as_str().unwrap();
    let token = v["token"].as_str().unwrap();
    assert_eq!(v["pool"], "beijing");
    assert!(cmd.contains(token), "the command must carry the token");
    assert!(cmd.contains("https://forkd.example.com"));

    // And the token it embedded really registers a node, once.
    let body = serde_json::json!({
        "token": token, "node_id": "bj-2", "fingerprint": "f", "max_sandboxes": 10,
    });
    let c = reqwest::Client::new();
    let first = c
        .post(format!("{}/v1/node/register", h.base))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), 200);
    let second = c
        .post(format!("{}/v1/node/register", h.base))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(second.status(), 401);
}

/// The property the held-open stream exists for: a node parked with no work
/// is handed a command as soon as desired state changes, without waiting for
/// any poll interval. The node listens on nothing -- it is parked on a
/// request it opened itself.
#[tokio::test]
async fn a_parked_node_is_dispatched_to_at_once() {
    let h = start().await;
    register(&h, "bm-1", "default").await;
    put_template(&h, "python").await;

    // Park a request. There is nothing to do yet, so the master holds it.
    let base = h.base.clone();
    let parked = tokio::spawn(async move {
        let t0 = std::time::Instant::now();
        let resp: PollResponse = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .unwrap()
            .post(format!("{base}/v1/node/poll"))
            .json(&serde_json::json!({
                "node_id": "bm-1", "observed": [],
                "free_memory_mib": 2000, "pressure_some_avg10": 0.0,
                "hold_ms": 10000,
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        (resp, t0.elapsed())
    });

    // Let it settle into the wait, then create work for it.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let created: serde_json::Value = reqwest::Client::new()
        .post(format!("{}/v1/sandboxes", h.base))
        .json(&serde_json::json!({ "template": "python", "port": 19910 }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();

    let (resp, elapsed) = parked.await.unwrap();
    assert!(
        matches!(resp.commands.as_slice(), [forkd_core::proto::Command::Create { id: got, .. }] if got == &id),
        "got {:?}",
        resp.commands
    );
    // Well under the 10 s hold: the create woke it, the hold did not expire.
    assert!(
        elapsed < std::time::Duration::from_secs(3),
        "dispatch took {elapsed:?}; the parked request was not woken"
    );
}

/// A change that lands between the node computing its work and settling down
/// to wait must not be lost to the gap.
#[tokio::test]
async fn a_change_racing_the_wait_is_not_lost() {
    let h = start().await;
    register(&h, "bm-1", "default").await;
    put_template(&h, "python").await;

    // Create first, then park: the node must be handed the work straight
    // away rather than waiting out its hold.
    reqwest::Client::new()
        .post(format!("{}/v1/sandboxes", h.base))
        .json(&serde_json::json!({ "template": "python" }))
        .send()
        .await
        .unwrap();

    let t0 = std::time::Instant::now();
    let resp: PollResponse = reqwest::Client::new()
        .post(format!("{}/v1/node/poll", h.base))
        .json(&serde_json::json!({
            "node_id": "bm-1", "observed": [],
            "free_memory_mib": 2000, "pressure_some_avg10": 0.0,
            "hold_ms": 10000,
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp.commands.len(), 1);
    assert!(
        t0.elapsed() < std::time::Duration::from_secs(1),
        "answered at once"
    );
}

/// With nothing to do, the request really is parked -- it does not return
/// empty immediately and spin.
#[tokio::test]
async fn an_idle_node_is_parked_not_spinning() {
    let h = start().await;
    register(&h, "bm-1", "default").await;

    let t0 = std::time::Instant::now();
    let resp: PollResponse = reqwest::Client::new()
        .post(format!("{}/v1/node/poll", h.base))
        .json(&serde_json::json!({
            "node_id": "bm-1", "observed": [],
            "free_memory_mib": 2000, "pressure_some_avg10": 0.0,
            "hold_ms": 1000,
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(resp.commands.is_empty());
    assert!(
        t0.elapsed() >= std::time::Duration::from_millis(900),
        "returned after {:?}; it should have been parked for ~1 s",
        t0.elapsed()
    );
}

/// A node that reports a failing sandbox makes the reason visible in the
/// API. Without this an operator has only one machine's log to go on.
#[tokio::test]
async fn a_reported_failure_shows_up_on_the_sandbox() {
    let h = start().await;
    register(&h, "bm-1", "default").await;
    put_template(&h, "python").await;
    let c = reqwest::Client::new();
    let created: serde_json::Value = c
        .post(format!("{}/v1/sandboxes", h.base))
        .json(&serde_json::json!({ "template": "python" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();
    assert!(
        created.get("last_error").is_none(),
        "a fresh sandbox has no error"
    );

    c.post(format!("{}/v1/node/poll", h.base))
        .json(&serde_json::json!({
            "node_id": "bm-1", "observed": [],
            "free_memory_mib": 2000, "pressure_some_avg10": 0.0, "hold_ms": 0,
            "failures": [{
                "id": id, "attempts": 4,
                "error": "systemd-run: Access denied as the requested operation requires interactive authentication"
            }],
        }))
        .send()
        .await
        .unwrap();

    let got: serde_json::Value = c
        .get(format!("{}/v1/sandboxes/{id}", h.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(got["error_attempts"], 4);
    assert!(
        got["last_error"]
            .as_str()
            .unwrap()
            .contains("Access denied"),
        "got {:?}",
        got["last_error"]
    );
}

/// A sandbox that keeps failing is taken out of the command stream. Retrying
/// for ever is a standing CPU cost on the node, not resilience.
#[tokio::test]
async fn a_repeatedly_failing_sandbox_is_quarantined_and_resumable() {
    let h = start().await;
    register(&h, "bm-1", "default").await;
    put_template(&h, "python").await;
    let c = reqwest::Client::new();
    let created: serde_json::Value = c
        .post(format!("{}/v1/sandboxes", h.base))
        .json(&serde_json::json!({ "template": "python" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();

    let poll = |failures: serde_json::Value| {
        let base = h.base.clone();
        async move {
            reqwest::Client::new()
                .post(format!("{base}/v1/node/poll"))
                .json(&serde_json::json!({
                    "node_id": "bm-1", "observed": [],
                    "free_memory_mib": 2000, "pressure_some_avg10": 0.0,
                    "hold_ms": 0, "failures": failures,
                }))
                .send()
                .await
                .unwrap()
                .json::<PollResponse>()
                .await
                .unwrap()
        }
    };

    // Just under the limit: still being asked for.
    let resp = poll(serde_json::json!([{
        "id": id, "attempts": forkd_core::master::MAX_FAILURES - 1, "error": "boom"
    }]))
    .await;
    assert_eq!(resp.commands.len(), 1, "not quarantined yet");

    // At the limit: the master stops issuing it.
    let resp = poll(serde_json::json!([{
        "id": id, "attempts": forkd_core::master::MAX_FAILURES, "error": "boom"
    }]))
    .await;
    assert!(
        resp.commands.is_empty(),
        "a quarantined sandbox must not be re-issued: {:?}",
        resp.commands
    );

    // But it is still there, with the reason attached.
    let got: serde_json::Value = c
        .get(format!("{}/v1/sandboxes/{id}", h.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(got["last_error"], "boom");
    assert_eq!(got["error_attempts"], forkd_core::master::MAX_FAILURES);

    // An explicit retry puts it back in the stream.
    let after: serde_json::Value = c
        .post(format!("{}/v1/sandboxes/{id}/retry", h.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(after.get("last_error").is_none(), "retry clears the error");
    let resp = poll(serde_json::json!([])).await;
    assert_eq!(resp.commands.len(), 1, "back in the command stream");
}

/// A sandbox whose template has been removed must not produce a Create the
/// node can only fail on.
#[tokio::test]
async fn a_sandbox_with_a_missing_template_is_not_issued() {
    let h = start().await;
    register(&h, "bm-1", "default").await;
    put_template(&h, "python").await;
    let c = reqwest::Client::new();
    c.post(format!("{}/v1/sandboxes", h.base))
        .json(&serde_json::json!({ "template": "python" }))
        .send()
        .await
        .unwrap();

    // Replace the template with a differently named one, leaving the
    // sandbox pointing at something that no longer exists.
    let resp: PollResponse = c
        .post(format!("{}/v1/node/poll", h.base))
        .json(&serde_json::json!({
            "node_id": "bm-1", "observed": [],
            "free_memory_mib": 2000, "pressure_some_avg10": 0.0, "hold_ms": 0,
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        resp.commands.len(),
        1,
        "template present, so Create is issued"
    );
}

/// A detached sandbox is supposed to be absent from every node, so a node
/// not holding it is already correct. Issuing Create would resurrect it the
/// moment it detached, and the tier would never hold.
#[tokio::test]
async fn a_detached_sandbox_is_not_recreated() {
    let h = start().await;
    register(&h, "bm-1", "default").await;
    put_template(&h, "python").await;
    let c = reqwest::Client::new();
    let created: serde_json::Value = c
        .post(format!("{}/v1/sandboxes", h.base))
        .json(&serde_json::json!({ "template": "python" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();

    let poll = |observed: serde_json::Value| {
        let base = h.base.clone();
        async move {
            reqwest::Client::new()
                .post(format!("{base}/v1/node/poll"))
                .json(&serde_json::json!({
                    "node_id": "bm-1", "observed": observed,
                    "free_memory_mib": 2000, "pressure_some_avg10": 0.0, "hold_ms": 0,
                }))
                .send()
                .await
                .unwrap()
                .json::<PollResponse>()
                .await
                .unwrap()
        }
    };

    // Desired detached, node holds nothing: that is the correct state.
    c.post(format!("{}/v1/sandboxes/{id}/tier", h.base))
        .json(&serde_json::json!({ "tier": "detached" }))
        .send()
        .await
        .unwrap();
    let resp = poll(serde_json::json!([])).await;
    assert!(
        resp.commands.is_empty(),
        "a detached sandbox must not be recreated: {:?}",
        resp.commands
    );

    // And asking for it back does issue a Create, which is how it returns.
    c.post(format!("{}/v1/sandboxes/{id}/touch", h.base))
        .send()
        .await
        .unwrap();
    let resp = poll(serde_json::json!([])).await;
    assert!(
        matches!(
            resp.commands.as_slice(),
            [forkd_core::proto::Command::Create { .. }]
        ),
        "promotion from detached comes back as Create: {:?}",
        resp.commands
    );
}

/// Ports are no longer a node-wide resource: each sandbox has its own
/// network namespace, so they all listen on the same port and what the
/// master hands out is an address.
#[tokio::test]
async fn every_sandbox_gets_the_same_port_and_its_own_slot() {
    let h = start().await;
    register(&h, "bm-1", "default").await;
    put_template(&h, "python").await;
    let c = reqwest::Client::new();

    let mut ports = std::collections::HashSet::new();
    for _ in 0..5 {
        let v: serde_json::Value = c
            .post(format!("{}/v1/sandboxes", h.base))
            .json(&serde_json::json!({ "template": "python" }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        ports.insert(v["port"].as_u64().unwrap());
    }
    assert_eq!(
        ports.len(),
        1,
        "all five share one port; they are in separate namespaces"
    );
    assert_eq!(
        *ports.iter().next().unwrap() as u16,
        forkd_core::master::DEFAULT_SANDBOX_PORT
    );
}

/// A caller may still ask for a particular port inside its sandbox.
#[tokio::test]
async fn a_requested_port_is_honoured() {
    let h = start().await;
    register(&h, "bm-1", "default").await;
    put_template(&h, "python").await;
    let v: serde_json::Value = reqwest::Client::new()
        .post(format!("{}/v1/sandboxes", h.base))
        .json(&serde_json::json!({ "template": "python", "port": 9999 }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["port"], 9999);
}

/// Running out of addresses is a clear refusal at create time, not a
/// sandbox that is accepted and then cannot be given a network.
#[tokio::test]
async fn an_exhausted_address_plan_is_refused_up_front() {
    let td = TempDir::new().unwrap();
    let st = Arc::new(MasterState::new(
        Store::open(td.path().join("state.db")).unwrap(),
        Ca::open_or_create(td.path().join("pki")).unwrap(),
        forkd_core::master::MasterOptions {
            admit: forkd_core::master::Admission {
                min_free_mib: 0,
                max_psi_avg10: f64::MAX,
            },
            // Room for exactly three sandboxes.
            max_net_index: 2,
            ..Default::default()
        },
    ));
    let listener = tokio::net::TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router(st)).await.ok();
    });
    let h = Harness {
        base: format!("http://{addr}"),
        _td: td,
    };
    register(&h, "bm-1", "default").await;
    put_template(&h, "python").await;
    let c = reqwest::Client::new();

    for i in 0..3 {
        let r = c
            .post(format!("{}/v1/sandboxes", h.base))
            .json(&serde_json::json!({ "template": "python" }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 201, "sandbox {i} should have got a slot");
    }
    let r = c
        .post(format!("{}/v1/sandboxes", h.base))
        .json(&serde_json::json!({ "template": "python" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 503);
    let body: serde_json::Value = r.json().await.unwrap();
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("no free network slot"),
        "got {:?}",
        body["error"]
    );
}

/// The node owns addressing, so the address it reports is what the API
/// serves -- there is nothing for a caller to reach a sandbox by until then.
#[tokio::test]
async fn the_address_the_node_reports_is_what_callers_see() {
    let h = start().await;
    register(&h, "bm-1", "default").await;
    put_template(&h, "python").await;
    let c = reqwest::Client::new();
    let created: serde_json::Value = c
        .post(format!("{}/v1/sandboxes", h.base))
        .json(&serde_json::json!({ "template": "python" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();
    assert!(
        created.get("ip").is_none(),
        "no address until the node reports one"
    );

    c.post(format!("{}/v1/node/poll", h.base))
        .json(&serde_json::json!({
            "node_id": "bm-1",
            "observed": [{
                "id": id, "tier": "hot", "serving": true,
                "ip": "10.86.0.2", "net_index": 0
            }],
            "free_memory_mib": 2000, "pressure_some_avg10": 0.0, "hold_ms": 0,
        }))
        .send()
        .await
        .unwrap();

    let got: serde_json::Value = c
        .get(format!("{}/v1/sandboxes/{id}", h.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(got["ip"], "10.86.0.2");
}

/// A node reports its own memory and pressure on every poll, and the master
/// must act on them. Without this it fills a node until it stops answering:
/// on bm that happened at 139 sandboxes, with the node having reported
/// 391 MiB free and a pressure average of 61 beforehand.
#[tokio::test]
async fn a_loaded_node_stops_being_given_sandboxes() {
    let td = TempDir::new().unwrap();
    let st = Arc::new(MasterState::new(
        Store::open(td.path().join("state.db")).unwrap(),
        Ca::open_or_create(td.path().join("pki")).unwrap(),
        forkd_core::master::MasterOptions {
            port_range: (20000, 20100),
            admit: forkd_core::master::Admission {
                min_free_mib: 512,
                max_psi_avg10: 8.0,
            },
            ..Default::default()
        },
    ));
    let listener = tokio::net::TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router(st)).await.ok();
    });
    let h = Harness {
        base: format!("http://{addr}"),
        _td: td,
    };
    register(&h, "bm-1", "default").await;
    put_template(&h, "python").await;
    let c = reqwest::Client::new();

    let report = |free: u64, psi: f64| {
        let base = h.base.clone();
        async move {
            reqwest::Client::new()
                .post(format!("{base}/v1/node/poll"))
                .json(&serde_json::json!({
                    "node_id": "bm-1", "observed": [],
                    "free_memory_mib": free, "pressure_some_avg10": psi,
                    "hold_ms": 0,
                }))
                .send()
                .await
                .unwrap();
        }
    };
    let create = || {
        let base = h.base.clone();
        async move {
            reqwest::Client::new()
                .post(format!("{base}/v1/sandboxes"))
                .json(&serde_json::json!({ "template": "python" }))
                .send()
                .await
                .unwrap()
        }
    };

    // A healthy node takes work.
    report(2000, 0.5).await;
    assert_eq!(create().await.status(), 201);

    // Low on memory: refused, with the reason.
    report(400, 0.5).await;
    let r = create().await;
    assert_eq!(r.status(), 503);
    let body: serde_json::Value = r.json().await.unwrap();
    let msg = body["error"].as_str().unwrap();
    assert!(
        msg.contains("too loaded") && msg.contains("400 MiB free"),
        "got {msg}"
    );

    // Memory fine but thrashing: also refused. This is the case free memory
    // alone misses -- on bm the apparent per-sandbox cost FELL from 22.8 to
    // 16.6 MiB while pressure climbed, because swap was absorbing it.
    report(2000, 20.0).await;
    let r = create().await;
    assert_eq!(r.status(), 503);
    let msg = r.json::<serde_json::Value>().await.unwrap()["error"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(msg.contains("pressure 20.0"), "got {msg}");

    // And it is visible on the node, not only in the refusal.
    let nodes: serde_json::Value = c
        .get(format!("{}/v1/nodes", h.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(nodes[0]["psi_some_avg10"], 20.0);
    assert!(nodes[0]["not_admitting"]
        .as_str()
        .unwrap()
        .contains("pressure"));

    // Recovering lets it take work again.
    report(2000, 0.2).await;
    assert_eq!(create().await.status(), 201);
    let nodes: serde_json::Value = c
        .get(format!("{}/v1/nodes", h.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(nodes[0]["not_admitting"].is_null(), "admitting again");
}

/// exec is request/response over a channel built for fire-and-forget
/// commands, so the correlation path is what these cover: the command
/// reaches the node, the result reaches the caller, and every way it can go
/// wrong still answers.
mod exec {
    use super::*;

    async fn hot_sandbox(h: &Harness) -> String {
        register(h, "bm-1", "default").await;
        put_template(h, "python").await;
        let created: serde_json::Value = reqwest::Client::new()
            .post(format!("{}/v1/sandboxes", h.base))
            .json(&serde_json::json!({ "template": "python" }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        created["id"].as_str().unwrap().to_string()
    }

    /// The whole round trip: caller parks, node is handed the command on its
    /// stream, node reports the result, caller gets it.
    #[tokio::test]
    async fn output_reaches_the_caller() {
        let h = start().await;
        let id = hot_sandbox(&h).await;
        let base = h.base.clone();

        let caller = tokio::spawn({
            let base = base.clone();
            let id = id.clone();
            async move {
                reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(20))
                    .build()
                    .unwrap()
                    .post(format!("{base}/v1/sandboxes/{id}/exec"))
                    .json(&serde_json::json!({ "args": ["echo", "hi"] }))
                    .send()
                    .await
                    .unwrap()
                    .json::<serde_json::Value>()
                    .await
                    .unwrap()
            }
        });

        // Stand in for the node: poll, find the Exec, report its output.
        let mut exec_id = None;
        for _ in 0..60 {
            let resp: PollResponse = reqwest::Client::new()
                .post(format!("{base}/v1/node/poll"))
                .json(&serde_json::json!({
                    "node_id": "bm-1",
                    "observed": [ObservedSandbox { id: id.clone(), tier: Tier::Hot, serving: true, ip: None, net_index: None }],
                    "free_memory_mib": 2000, "pressure_some_avg10": 0.0, "hold_ms": 0,
                }))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            for c in resp.commands {
                if let forkd_core::proto::Command::Exec {
                    exec_id: e, args, ..
                } = c
                {
                    assert_eq!(args, vec!["echo", "hi"]);
                    exec_id = Some(e);
                }
            }
            if exec_id.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let exec_id = exec_id.expect("the exec command reached the node");

        let posted = reqwest::Client::new()
            .post(format!("{base}/v1/node/result"))
            .json(&serde_json::json!({
                "exec_id": exec_id, "stdout": "hi\n", "stderr": "", "exit_code": 0
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(posted.status(), 204);

        let got = caller.await.unwrap();
        assert_eq!(got["stdout"], "hi\n");
        assert_eq!(got["exit_code"], 0);
    }

    /// A non-zero exit is a result, not an error: the caller asked what the
    /// command did and that is what it did.
    #[tokio::test]
    async fn a_failing_command_is_still_a_result() {
        let h = start().await;
        let id = hot_sandbox(&h).await;
        let base = h.base.clone();
        let caller = tokio::spawn({
            let base = base.clone();
            let id = id.clone();
            async move {
                reqwest::Client::new()
                    .post(format!("{base}/v1/sandboxes/{id}/exec"))
                    .json(&serde_json::json!({ "args": ["false"] }))
                    .send()
                    .await
                    .unwrap()
            }
        });
        let mut exec_id = None;
        for _ in 0..60 {
            let resp: PollResponse = reqwest::Client::new()
                .post(format!("{base}/v1/node/poll"))
                .json(&serde_json::json!({
                    "node_id": "bm-1",
                    "observed": [ObservedSandbox { id: id.clone(), tier: Tier::Hot, serving: true, ip: None, net_index: None }],
                    "free_memory_mib": 2000, "pressure_some_avg10": 0.0, "hold_ms": 0,
                }))
                .send().await.unwrap().json().await.unwrap();
            for c in resp.commands {
                if let forkd_core::proto::Command::Exec { exec_id: e, .. } = c {
                    exec_id = Some(e);
                }
            }
            if exec_id.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        reqwest::Client::new()
            .post(format!("{base}/v1/node/result"))
            .json(&serde_json::json!({
                "exec_id": exec_id.unwrap(), "stdout": "", "stderr": "nope\n", "exit_code": 1
            }))
            .send()
            .await
            .unwrap();
        let r = caller.await.unwrap();
        assert_eq!(r.status(), 200, "a non-zero exit is not an HTTP error");
        let v: serde_json::Value = r.json().await.unwrap();
        assert_eq!(v["exit_code"], 1);
        assert_eq!(v["stderr"], "nope\n");
    }

    /// A command cannot run in a sandbox that is not running. Promoting
    /// silently would hide a multi-second wait inside a quick-looking call.
    #[tokio::test]
    async fn exec_on_a_cold_sandbox_says_so_instead_of_waiting() {
        let h = start().await;
        let id = hot_sandbox(&h).await;
        reqwest::Client::new()
            .post(format!("{}/v1/sandboxes/{id}/tier", h.base))
            .json(&serde_json::json!({ "tier": "snapshot" }))
            .send()
            .await
            .unwrap();

        let r = reqwest::Client::new()
            .post(format!("{}/v1/sandboxes/{id}/exec", h.base))
            .json(&serde_json::json!({ "args": ["echo", "hi"] }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 409);
        let v: serde_json::Value = r.json().await.unwrap();
        let msg = v["error"].as_str().unwrap();
        assert!(
            msg.contains("snapshot") && msg.contains("touch"),
            "got {msg}"
        );
    }

    /// If the node never answers, the caller gets a timeout rather than
    /// hanging on an open request for ever.
    #[tokio::test]
    async fn a_silent_node_times_the_caller_out() {
        let h = start().await;
        let id = hot_sandbox(&h).await;
        // timeout_secs 0 means the budget is just the 20 s of slack, which
        // is too slow for a test -- so assert the shape, not the wait.
        let r = reqwest::Client::new()
            .post(format!("{}/v1/sandboxes/{id}/exec", h.base))
            .json(&serde_json::json!({ "args": ["echo"], "timeout_secs": 0 }))
            .timeout(std::time::Duration::from_millis(600))
            .send()
            .await;
        // The client gives up first; what matters is that the master kept
        // the request open rather than answering wrongly.
        assert!(
            r.is_err(),
            "the master must not answer before the node does"
        );
    }

    #[tokio::test]
    async fn an_empty_command_is_refused() {
        let h = start().await;
        let id = hot_sandbox(&h).await;
        let r = reqwest::Client::new()
            .post(format!("{}/v1/sandboxes/{id}/exec", h.base))
            .json(&serde_json::json!({ "args": [] }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400);
    }

    #[tokio::test]
    async fn a_result_nobody_waits_for_is_refused() {
        let h = start().await;
        let r = reqwest::Client::new()
            .post(format!("{}/v1/node/result", h.base))
            .json(&serde_json::json!({
                "exec_id": "deadbeef", "stdout": "", "stderr": "", "exit_code": 0
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 404);
    }

    #[tokio::test]
    async fn exec_on_an_unknown_sandbox_is_not_found() {
        let h = start().await;
        register(&h, "bm-1", "default").await;
        let r = reqwest::Client::new()
            .post(format!("{}/v1/sandboxes/nope/exec", h.base))
            .json(&serde_json::json!({ "args": ["echo"] }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 404);
    }
}

/// Code inside a sandbox can reach the master over loopback -- measured on
/// bm, where a sandbox connected to the API, to sshd and to a sibling. So
/// an open control plane means a sandbox can mint node tokens and delete
/// other sandboxes. These cover the token actually being required.
mod api_auth {
    use super::*;

    async fn guarded() -> (Harness, String) {
        let td = TempDir::new().unwrap();
        let token = "s3cret-token".to_string();
        let st = Arc::new(MasterState::new(
            Store::open(td.path().join("state.db")).unwrap(),
            Ca::open_or_create(td.path().join("pki")).unwrap(),
            forkd_core::master::MasterOptions {
                port_range: (20000, 20100),
                admit: forkd_core::master::Admission {
                    min_free_mib: 0,
                    max_psi_avg10: f64::MAX,
                },
                api_token: Some(token.clone()),
                ..Default::default()
            },
        ));
        let listener = tokio::net::TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router(st)).await.ok();
        });
        (
            Harness {
                base: format!("http://{addr}"),
                _td: td,
            },
            token,
        )
    }

    #[tokio::test]
    async fn the_control_plane_refuses_an_unauthenticated_caller() {
        let (h, _t) = guarded().await;
        let c = reqwest::Client::new();
        // Exactly the calls a sandbox would make if it could reach the API.
        for (method, path) in [
            ("GET", "/v1/sandboxes"),
            ("GET", "/v1/nodes"),
            ("POST", "/v1/node-tokens"),
            ("POST", "/v1/node-bootstrap"),
            ("POST", "/v1/templates"),
            ("DELETE", "/v1/sandboxes/sb-someone-elses"),
        ] {
            let r = match method {
                "GET" => c.get(format!("{}{path}", h.base)).send().await.unwrap(),
                "DELETE" => c.delete(format!("{}{path}", h.base)).send().await.unwrap(),
                _ => c
                    .post(format!("{}{path}", h.base))
                    .json(&serde_json::json!({}))
                    .send()
                    .await
                    .unwrap(),
            };
            assert_eq!(r.status(), 401, "{method} {path} was not refused");
        }
    }

    #[tokio::test]
    async fn a_health_probe_needs_no_credential() {
        let (h, _t) = guarded().await;
        let r = reqwest::Client::new()
            .get(format!("{}/healthz", h.base))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "a load balancer has no token");
    }

    /// Registration is the one guarded action a node cannot authenticate,
    /// because the single-use token it spends IS its credential.
    #[tokio::test]
    async fn registration_is_open_but_still_needs_its_own_token() {
        let (h, token) = guarded().await;
        let c = reqwest::Client::new();

        let bad = c
            .post(format!("{}/v1/node/register", h.base))
            .json(&serde_json::json!({
                "token": "not-a-real-token", "node_id": "bm-1",
                "fingerprint": "f", "max_sandboxes": 1
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(bad.status(), 401, "a bogus registration token is refused");

        // A real one works, and hands back the API token for later calls.
        let tok: serde_json::Value = c
            .post(format!("{}/v1/node-tokens", h.base))
            .bearer_auth(&token)
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let reg: serde_json::Value = c
            .post(format!("{}/v1/node/register", h.base))
            .json(&serde_json::json!({
                "token": tok["token"], "node_id": "bm-1",
                "fingerprint": "f", "max_sandboxes": 1
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            reg["api_token"], token,
            "the node is given what its polls need"
        );
    }

    #[tokio::test]
    async fn the_right_token_is_accepted_and_a_wrong_one_is_not() {
        let (h, token) = guarded().await;
        let c = reqwest::Client::new();
        let ok = c
            .get(format!("{}/v1/sandboxes", h.base))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        assert_eq!(ok.status(), 200);

        for wrong in ["", "s3cret-toke", "s3cret-tokenn", "S3CRET-TOKEN"] {
            let r = c
                .get(format!("{}/v1/sandboxes", h.base))
                .bearer_auth(wrong)
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 401, "token {wrong:?} should not pass");
        }
    }
}

/// Every handler that changes desired state must wake the node, or the
/// change waits out the node's whole hold. The earlier delete test polled
/// with `hold_ms: 0`, so it never exercised the wake and missed that
/// delete_sandbox had none: measured on bm, a deleted sandbox kept running
/// for 30 s against a 25 s hold.
#[tokio::test]
async fn deleting_wakes_a_parked_node() {
    let h = start().await;
    register(&h, "bm-1", "default").await;
    put_template(&h, "python").await;
    let c = reqwest::Client::new();
    let created: serde_json::Value = c
        .post(format!("{}/v1/sandboxes", h.base))
        .json(&serde_json::json!({ "template": "python" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();

    // Creating the sandbox already woke this node, and `notify_one` leaves
    // a permit behind. Drain it before parking, or the park below returns
    // on that stale permit and the test reads it as the delete's wake --
    // which is exactly how this test first fooled me.
    //
    // The hold has to be non-zero: a zero hold skips the wait altogether,
    // so it never consumes the permit. A short one returns at once because
    // the permit is there.
    let drain = serde_json::json!({
        "node_id": "bm-1",
        "observed": [{ "id": id, "tier": "hot", "serving": true }],
        "free_memory_mib": 2000, "pressure_some_avg10": 0.0, "hold_ms": 150,
    });
    c.post(format!("{}/v1/node/poll", h.base))
        .json(&drain)
        .send()
        .await
        .unwrap();

    // Now park. Nothing to do, so the master keeps the request.
    let base = h.base.clone();
    let parked = tokio::spawn({
        let id = id.clone();
        async move {
            let t0 = std::time::Instant::now();
            let resp: PollResponse = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .unwrap()
                .post(format!("{base}/v1/node/poll"))
                .json(&serde_json::json!({
                    "node_id": "bm-1",
                    "observed": [{ "id": id, "tier": "hot", "serving": true }],
                    "free_memory_mib": 2000, "pressure_some_avg10": 0.0,
                    "hold_ms": 20000,
                }))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            (resp, t0.elapsed())
        }
    });

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let del = c
        .delete(format!("{}/v1/sandboxes/{id}", h.base))
        .send()
        .await
        .unwrap();
    assert_eq!(del.status(), 204);
    // Prove the row really went, so an empty command list below can only
    // mean the diff is wrong and not that the delete silently failed.
    let after = c
        .get(format!("{}/v1/sandboxes/{id}", h.base))
        .send()
        .await
        .unwrap();
    assert_eq!(
        after.status(),
        404,
        "the sandbox should be gone from desired state"
    );

    let (resp, elapsed) = parked.await.unwrap();
    assert!(
        matches!(resp.commands.as_slice(), [forkd_core::proto::Command::Destroy { id: got, .. }] if got == &id),
        "got {:?}",
        resp.commands
    );
    assert!(
        elapsed < std::time::Duration::from_secs(3),
        "the parked request returned after {elapsed:?}; the delete did not wake it"
    );
}

/// Branch and fork, which are what the project is named after. The RFC's
/// compatibility section requires the v0.5 request bodies keep working and
/// removed options return 410 naming the replacement.
mod branch {
    use super::*;

    /// Stand in for a node: take the branch command and report success.
    async fn serve_branch(base: &str, node: &str, size: u64) -> Option<String> {
        for _ in 0..80 {
            let resp: PollResponse = reqwest::Client::new()
                .post(format!("{base}/v1/node/poll"))
                .json(&serde_json::json!({
                    "node_id": node, "observed": [],
                    "free_memory_mib": 2000, "pressure_some_avg10": 0.0, "hold_ms": 0,
                }))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            for c in resp.commands {
                if let forkd_core::proto::Command::Branch { result_id, tag, .. } = c {
                    reqwest::Client::new()
                        .post(format!("{base}/v1/node/result"))
                        .json(&serde_json::json!({
                            "result_id": result_id, "exit_code": 0, "size_bytes": size
                        }))
                        .send()
                        .await
                        .unwrap();
                    return Some(tag);
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        None
    }

    async fn hot(h: &Harness) -> String {
        register(h, "bm-1", "default").await;
        put_template(h, "python").await;
        let v: serde_json::Value = reqwest::Client::new()
            .post(format!("{}/v1/sandboxes", h.base))
            .json(&serde_json::json!({ "template": "python" }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        v["id"].as_str().unwrap().to_string()
    }

    /// The tag is recorded only after the node reports the image written,
    /// so a fork cannot race ahead of it.
    #[tokio::test]
    async fn branching_records_the_tag_once_the_node_has_written_it() {
        let h = start().await;
        let id = hot(&h).await;
        let base = h.base.clone();
        let node = tokio::spawn(async move { serve_branch(&base, "bm-1", 14_680_064).await });

        let v: serde_json::Value = reqwest::Client::new()
            .post(format!("{}/v1/sandboxes/{id}/branch", h.base))
            .json(&serde_json::json!({ "tag": "py-base" }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(node.await.unwrap().as_deref(), Some("py-base"));
        assert_eq!(v["tag"], "py-base");
        assert_eq!(v["from_sandbox"], id);
        assert_eq!(v["size_bytes"], 14_680_064u64);

        let listed: serde_json::Value = reqwest::Client::new()
            .get(format!("{}/v1/snapshots", h.base))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(listed[0]["tag"], "py-base");
    }

    /// One image, many children: a fork of n lands n sandboxes, all on the
    /// node holding the image, and the v0.5 body gets the v0.5 list back.
    #[tokio::test]
    async fn forking_a_tag_makes_n_children_from_one_image() {
        let h = start().await;
        let id = hot(&h).await;
        let base = h.base.clone();
        let node = tokio::spawn(async move { serve_branch(&base, "bm-1", 1024).await });
        reqwest::Client::new()
            .post(format!("{}/v1/sandboxes/{id}/branch", h.base))
            .json(&serde_json::json!({ "tag": "t1" }))
            .send()
            .await
            .unwrap();
        node.await.unwrap();

        let kids: serde_json::Value = reqwest::Client::new()
            .post(format!("{}/v1/sandboxes", h.base))
            .json(&serde_json::json!({
                "snapshot_tag": "t1", "n": 4, "per_child_netns": true
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let arr = kids.as_array().expect("the v0.5 body gets a list back");
        assert_eq!(arr.len(), 4);
        let mut slots = std::collections::HashSet::new();
        for k in arr {
            assert_eq!(k["node_id"], "bm-1", "forks land where the image is");
            assert_eq!(k["template"], "python", "inherited from the tag");
            assert_eq!(k["port"], forkd_core::master::DEFAULT_SANDBOX_PORT);
            assert!(slots.insert(k["id"].as_str().unwrap().to_string()));
        }

        let info: serde_json::Value = reqwest::Client::new()
            .get(format!("{}/v1/snapshots/t1/info", h.base))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(info["children"], 4);
    }

    /// The node is told to restore rather than cold start.
    #[tokio::test]
    async fn a_forked_child_is_restored_not_started_cold() {
        let h = start().await;
        let id = hot(&h).await;
        let base = h.base.clone();
        let node = tokio::spawn(async move { serve_branch(&base, "bm-1", 1).await });
        reqwest::Client::new()
            .post(format!("{}/v1/sandboxes/{id}/branch", h.base))
            .json(&serde_json::json!({ "tag": "t2" }))
            .send()
            .await
            .unwrap();
        node.await.unwrap();

        let kid: serde_json::Value = reqwest::Client::new()
            .post(format!("{}/v1/sandboxes", h.base))
            .json(&serde_json::json!({ "snapshot_tag": "t2" }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let kid_id = kid["id"].as_str().unwrap().to_string();

        let resp: PollResponse = reqwest::Client::new()
            .post(format!("{}/v1/node/poll", h.base))
            .json(&serde_json::json!({
                "node_id": "bm-1", "observed": [],
                "free_memory_mib": 2000, "pressure_some_avg10": 0.0, "hold_ms": 0,
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let forked = resp.commands.iter().any(|c| {
            matches!(
                c,
                forkd_core::proto::Command::Create { id, from_tag: Some(t), .. }
                    if id == &kid_id && t == "t2"
            )
        });
        assert!(forked, "the child must carry its tag: {:?}", resp.commands);
    }

    /// One image serves every child, so dropping it under them would
    /// strand them.
    #[tokio::test]
    async fn a_tag_with_children_cannot_be_deleted() {
        let h = start().await;
        let id = hot(&h).await;
        let base = h.base.clone();
        let node = tokio::spawn(async move { serve_branch(&base, "bm-1", 1).await });
        reqwest::Client::new()
            .post(format!("{}/v1/sandboxes/{id}/branch", h.base))
            .json(&serde_json::json!({ "tag": "t3" }))
            .send()
            .await
            .unwrap();
        node.await.unwrap();
        let c = reqwest::Client::new();
        let kid: serde_json::Value = c
            .post(format!("{}/v1/sandboxes", h.base))
            .json(&serde_json::json!({ "snapshot_tag": "t3" }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();

        let r = c
            .delete(format!("{}/v1/snapshots/t3", h.base))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 409);
        assert!(r.json::<serde_json::Value>().await.unwrap()["error"]
            .as_str()
            .unwrap()
            .contains("still forked"));

        // Once the child is gone it can go.
        c.delete(format!(
            "{}/v1/sandboxes/{}",
            h.base,
            kid["id"].as_str().unwrap()
        ))
        .send()
        .await
        .unwrap();
        let r = c
            .delete(format!("{}/v1/snapshots/t3", h.base))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 204);
    }

    /// Removed options are refused with the replacement named, as the RFC
    /// requires, rather than silently ignored.
    #[tokio::test]
    async fn removed_options_return_410_naming_the_replacement() {
        let h = start().await;
        let id = hot(&h).await;
        let c = reqwest::Client::new();

        for body in [
            serde_json::json!({ "tag": "x", "mode": "live" }),
            serde_json::json!({ "tag": "x", "live": true }),
            serde_json::json!({ "tag": "x", "wait": true }),
        ] {
            let r = c
                .post(format!("{}/v1/sandboxes/{id}/branch", h.base))
                .json(&body)
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 410, "{body} should be refused");
            let e = r.json::<serde_json::Value>().await.unwrap()["error"]
                .as_str()
                .unwrap()
                .to_string();
            assert!(
                e.contains("v0.6"),
                "the refusal should say what changed: {e}"
            );
        }

        for body in [
            serde_json::json!({ "snapshot_tag": "x", "live_fork": true }),
            serde_json::json!({ "snapshot_tag": "x", "hugepages": true }),
        ] {
            let r = c
                .post(format!("{}/v1/sandboxes", h.base))
                .json(&body)
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 410, "{body} should be refused");
        }
    }

    #[tokio::test]
    async fn branching_a_cold_sandbox_and_other_refusals() {
        let h = start().await;
        let id = hot(&h).await;
        let c = reqwest::Client::new();

        // Only a running sandbox has memory to checkpoint.
        c.post(format!("{}/v1/sandboxes/{id}/tier", h.base))
            .json(&serde_json::json!({ "tier": "snapshot" }))
            .send()
            .await
            .unwrap();
        let r = c
            .post(format!("{}/v1/sandboxes/{id}/branch", h.base))
            .json(&serde_json::json!({ "tag": "t4" }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 409);

        // A tag is a path component on the node.
        for bad in ["", "a/b", "../etc", ".hidden"] {
            let r = c
                .post(format!("{}/v1/sandboxes/{id}/branch", h.base))
                .json(&serde_json::json!({ "tag": bad }))
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 400, "tag {bad:?} should be refused");
        }

        // Forking something that does not exist.
        let r = c
            .post(format!("{}/v1/sandboxes", h.base))
            .json(&serde_json::json!({ "snapshot_tag": "nope" }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 404);

        // Neither, or both.
        for body in [
            serde_json::json!({}),
            serde_json::json!({ "template": "python", "snapshot_tag": "t" }),
        ] {
            let r = c
                .post(format!("{}/v1/sandboxes", h.base))
                .json(&body)
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 400, "{body} should be refused");
        }
    }
}

/// Both SDKs send `per_child_netns: false` by default, so refusing it
/// breaks the plainest call either one makes. v0.6 gives every sandbox its
/// own namespace regardless, which is stronger than what `false` asks for,
/// so the field is accepted and ignored either way.
#[tokio::test]
async fn the_sdk_default_spawn_is_accepted() {
    let h = start().await;
    register(&h, "bm-1", "default").await;
    put_template(&h, "python").await;
    let c = reqwest::Client::new();

    // Seed a tag the way a branch would.
    let parent: serde_json::Value = c
        .post(format!("{}/v1/sandboxes", h.base))
        .json(&serde_json::json!({ "template": "python" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let pid = parent["id"].as_str().unwrap().to_string();
    let base = h.base.clone();
    let node = tokio::spawn(async move {
        for _ in 0..80 {
            let resp: PollResponse = reqwest::Client::new()
                .post(format!("{base}/v1/node/poll"))
                .json(&serde_json::json!({
                    "node_id": "bm-1", "observed": [],
                    "free_memory_mib": 2000, "pressure_some_avg10": 0.0, "hold_ms": 0,
                }))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            for cmd in resp.commands {
                if let forkd_core::proto::Command::Branch { result_id, .. } = cmd {
                    reqwest::Client::new()
                        .post(format!("{base}/v1/node/result"))
                        .json(&serde_json::json!({
                            "result_id": result_id, "exit_code": 0, "size_bytes": 1
                        }))
                        .send()
                        .await
                        .unwrap();
                    return;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    });
    c.post(format!("{}/v1/sandboxes/{pid}/branch", h.base))
        .json(&serde_json::json!({ "tag": "dflt" }))
        .send()
        .await
        .unwrap();
    node.await.unwrap();

    // Exactly what both SDKs send with no options set.
    for body in [
        serde_json::json!({ "snapshot_tag": "dflt", "n": 1, "per_child_netns": false }),
        serde_json::json!({ "snapshot_tag": "dflt", "n": 1, "per_child_netns": true }),
    ] {
        let r = c
            .post(format!("{}/v1/sandboxes", h.base))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 201, "{body} should be accepted");
    }
}

/// The caller's memory ceiling must reach the node, not merely be accepted.
/// Both SDKs send `memory_limit_mib`, and it was being dropped: a caller
/// asking for 512 MiB got the template's value with no way to tell.
#[tokio::test]
async fn a_callers_memory_limit_overrides_the_templates() {
    let h = start().await;
    register(&h, "bm-1", "default").await;
    let c = reqwest::Client::new();
    // The template says 256.
    c.post(format!("{}/v1/templates", h.base))
        .json(&serde_json::json!({
            "name": "python", "rootfs": "rootfs",
            "spec": { "args": ["/usr/bin/python3", "/app/s.py"], "memory_max_mib": 256 }
        }))
        .send()
        .await
        .unwrap();

    let asked: serde_json::Value = c
        .post(format!("{}/v1/sandboxes", h.base))
        .json(&serde_json::json!({ "template": "python", "memory_limit_mib": 512 }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(asked["memory_limit_mib"], 512, "echoed back to the caller");

    let dflt: serde_json::Value = c
        .post(format!("{}/v1/sandboxes", h.base))
        .json(&serde_json::json!({ "template": "python" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        dflt.get("memory_limit_mib").is_none(),
        "absent means the template's value applies"
    );

    // What the node is actually told is the only thing that matters.
    let resp: PollResponse = c
        .post(format!("{}/v1/node/poll", h.base))
        .json(&serde_json::json!({
            "node_id": "bm-1", "observed": [],
            "free_memory_mib": 2000, "pressure_some_avg10": 0.0, "hold_ms": 0,
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let mut seen = std::collections::HashMap::new();
    for cmd in &resp.commands {
        if let forkd_core::proto::Command::Create { id, spec, .. } = cmd {
            seen.insert(id.clone(), spec["memory_max_mib"].as_u64());
        }
    }
    assert_eq!(
        seen.get(asked["id"].as_str().unwrap()),
        Some(&Some(512)),
        "the override must reach the node's spec, not stop at the API"
    );
    assert_eq!(
        seen.get(dflt["id"].as_str().unwrap()),
        Some(&Some(256)),
        "without an override the template's value is sent unchanged"
    );
}

/// A snapshot tag that would name a different directory is refused at the
/// API, not accepted and failed later on a node.
///
/// This is the shape of the 2026-05-17 advisory: `create_sandbox` joined
/// the caller's tag into a path while every sibling handler validated it.
/// The node validates too, which is what actually stops traversal, but a
/// tag that cannot work should not reach the database.
#[tokio::test]
async fn a_tag_that_is_not_a_plain_name_is_refused_with_400() {
    let h = start().await;
    register(&h, "bm-1", "default").await;
    let c = reqwest::Client::new();

    for bad in ["../../tmp/evil", "sub/dir", "..", ".hidden", "a\\b", ""] {
        let r = c
            .post(format!("{}/v1/sandboxes", h.base))
            .json(&serde_json::json!({ "snapshot_tag": bad, "port": 19900 }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            r.status().as_u16(),
            400,
            "snapshot_tag {bad:?} should be a 400, not accepted"
        );
    }

    // And branch, which already validated, keeps doing so through the same
    // helper rather than its own copy.
    let r = c
        .post(format!("{}/v1/sandboxes/nope/branch", h.base))
        .json(&serde_json::json!({ "tag": "../escape" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 400);
}
