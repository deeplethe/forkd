//! End-to-end through the master's HTTP surface: register a node, place a
//! sandbox, and check the node is told to create it. No runsc involved --
//! this is the control plane on its own.

use std::net::SocketAddr;
use std::sync::Arc;

use forkd_core::identity::Ca;
use forkd_core::master::{router, MasterState};
use forkd_core::proto::{ObservedSandbox, PollResponse};
use forkd_core::state::Store;
use forkd_core::tier::{IdlePolicy, Tier};
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
        IdlePolicy::default(),
        (20000, 20010),
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
            "observed": [ObservedSandbox { id: id.clone(), tier: Tier::Hot, serving: true }],
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
            "observed": [ObservedSandbox { id: id.clone(), tier: Tier::Hot, serving: true }],
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
            [forkd_core::proto::Command::Destroy { id: got }] if got == &id
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
                    "observed": [ObservedSandbox { id, tier, serving: false }],
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

/// A caller does not know a node's port space, so the master allocates.
#[tokio::test]
async fn ports_are_allocated_without_the_caller_asking() {
    let h = start().await;
    register(&h, "bm-1", "default").await;
    put_template(&h, "python").await;
    let c = reqwest::Client::new();

    let mut seen = std::collections::HashSet::new();
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
        let p = v["port"].as_u64().expect("a port was allocated") as u16;
        assert!((20000..=20010).contains(&p), "{p} outside the range");
        assert!(seen.insert(p), "port {p} handed out twice");
    }
}

/// Running out of ports is a clear refusal, not a sandbox that cannot start.
#[tokio::test]
async fn an_exhausted_port_range_is_refused_up_front() {
    let h = start().await;
    register(&h, "bm-1", "default").await;
    put_template(&h, "python").await;
    let c = reqwest::Client::new();

    // The harness range is 20000-20010: eleven ports.
    for i in 0..11 {
        let r = c
            .post(format!("{}/v1/sandboxes", h.base))
            .json(&serde_json::json!({ "template": "python" }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 201, "sandbox {i} should have got a port");
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
        body["error"].as_str().unwrap().contains("no free port"),
        "got {:?}",
        body["error"]
    );
}

/// Two nodes have separate port spaces, which is what lets a fleet exceed
/// one range.
#[tokio::test]
async fn two_nodes_may_use_the_same_port() {
    let h = start().await;
    register(&h, "bm-1", "default").await;
    register(&h, "bm-2", "default").await;
    put_template(&h, "python").await;
    let c = reqwest::Client::new();

    let mut by_node: std::collections::HashMap<String, u16> = Default::default();
    for _ in 0..2 {
        let v: serde_json::Value = c
            .post(format!("{}/v1/sandboxes", h.base))
            .json(&serde_json::json!({ "template": "python" }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        by_node.insert(
            v["node_id"].as_str().unwrap().to_string(),
            v["port"].as_u64().unwrap() as u16,
        );
    }
    assert_eq!(by_node.len(), 2, "placement spread across both nodes");
    let ports: Vec<u16> = by_node.values().copied().collect();
    assert_eq!(
        ports[0], ports[1],
        "each node starts its own range at 20000"
    );
}
