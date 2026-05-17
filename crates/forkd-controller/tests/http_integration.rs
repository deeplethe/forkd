//! Real-HTTP integration tests for forkd-controller.
//!
//! Unlike the unit tests in `src/http.rs` that use axum's in-process
//! `oneshot` Service, these spin up the actual daemon on a TCP port and
//! exercise it over a real HTTP/1.1 client. Catches things that only
//! show up at the wire boundary: TCP bind races, body framing, serde
//! round-trips end-to-end, content-type headers from real responses.
//!
//! Not gated behind `#[ignore]` — they don't touch /sys/fs/cgroup or
//! spawn Firecracker, so they pass on any Linux CI runner.

use forkd_controller::{run_daemon, DaemonConfig};
use serde_json::Value;
use std::net::{SocketAddr, TcpListener};
use std::time::Duration;
use tempfile::TempDir;

/// Probe an unused TCP port by binding ephemerally and immediately
/// dropping the listener. Inherently racy across processes, but the
/// daemon binds < 1ms after we drop, and there's only one of these per
/// test invocation, so collisions in CI are vanishingly rare.
fn pick_free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

struct TestDaemon {
    base: String,
    _shutdown: tokio::sync::oneshot::Sender<()>,
    _td: TempDir,
}

impl TestDaemon {
    async fn start() -> Self {
        Self::start_with(None).await
    }

    async fn start_with_token(token: &str) -> Self {
        Self::start_with(Some(token.to_string())).await
    }

    async fn start_with(token: Option<String>) -> Self {
        let port = pick_free_port();
        let bind: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let td = TempDir::new().unwrap();
        let token_file = token.map(|t| {
            let p = td.path().join("token");
            std::fs::write(&p, t).unwrap();
            p
        });
        let cfg = DaemonConfig {
            bind,
            state_file: td.path().join("state.json"),
            snapshot_root: td.path().join("snapshots"),
            audit_log: td.path().join("audit.log"),
            token_file,
            tls_cert: None,
            tls_key: None,
        };

        // We don't have a clean shutdown hook from outside (run_daemon
        // listens for SIGTERM internally), so we just abort the task on
        // drop. axum::serve gracefully closes when its listener drops.
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let _handle = tokio::spawn(async move {
            tokio::select! {
                _ = run_daemon(cfg) => {},
                _ = rx => {},
            }
        });

        let base = format!("http://127.0.0.1:{port}");

        // Wait up to 2s for /healthz.
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if reqwest::get(format!("{base}/healthz")).await.is_ok() {
                return Self {
                    base,
                    _shutdown: tx,
                    _td: td,
                };
            }
        }
        panic!("daemon never became reachable on {base}");
    }
}

#[tokio::test]
async fn end_to_end_healthz_and_metrics() {
    let d = TestDaemon::start().await;

    let h: Value = reqwest::get(format!("{}/healthz", d.base))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(h["ok"], serde_json::Value::Bool(true));

    let m = reqwest::get(format!("{}/metrics", d.base)).await.unwrap();
    assert_eq!(m.status(), 200);
    let body = m.text().await.unwrap();
    assert!(body.contains("forkd_sandboxes_active 0"));
    assert!(body.contains("forkd_build_info"));
}

#[tokio::test]
async fn end_to_end_404_for_missing_snapshot() {
    let d = TestDaemon::start().await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/sandboxes", d.base))
        .json(&serde_json::json!({"snapshot_tag": "does-not-exist", "n": 1}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let body: Value = resp.json().await.unwrap();
    assert!(body["error"]
        .as_str()
        .unwrap_or("")
        .contains("does-not-exist"));
}

#[tokio::test]
async fn end_to_end_version_round_trips() {
    let d = TestDaemon::start().await;
    let v: Value = reqwest::get(format!("{}/version", d.base))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["api"], "v1");
    assert!(v["version"].as_str().is_some());
}

#[tokio::test]
async fn end_to_end_auth_rejects_missing_token() {
    let d = TestDaemon::start_with_token("s3cret-test-token-1234567890").await;
    // /healthz is intentionally exempt — load balancers must probe.
    let h = reqwest::get(format!("{}/healthz", d.base)).await.unwrap();
    assert_eq!(h.status(), 200);
    // Any other route requires the bearer.
    let v = reqwest::get(format!("{}/version", d.base)).await.unwrap();
    assert_eq!(v.status(), 401);
}

#[tokio::test]
async fn end_to_end_auth_accepts_valid_token() {
    let d = TestDaemon::start_with_token("s3cret-test-token-1234567890").await;
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{}/version", d.base))
        .bearer_auth("s3cret-test-token-1234567890")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["api"], "v1");
}

struct TlsTestDaemon {
    base: String,
    _shutdown: tokio::sync::oneshot::Sender<()>,
    _td: TempDir,
}

impl TlsTestDaemon {
    async fn start() -> Self {
        // rcgen produces a fresh, self-signed CA-less leaf cert for
        // 127.0.0.1 + localhost. Good enough for an in-process test
        // that pairs it with reqwest's danger_accept_invalid_certs.
        let cert = rcgen::generate_simple_self_signed(vec![
            "127.0.0.1".to_string(),
            "localhost".to_string(),
        ])
        .unwrap();
        let td = TempDir::new().unwrap();
        let cert_path = td.path().join("cert.pem");
        let key_path = td.path().join("key.pem");
        std::fs::write(&cert_path, cert.cert.pem()).unwrap();
        std::fs::write(&key_path, cert.key_pair.serialize_pem()).unwrap();

        let port = pick_free_port();
        let bind: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let cfg = DaemonConfig {
            bind,
            state_file: td.path().join("state.json"),
            snapshot_root: td.path().join("snapshots"),
            audit_log: td.path().join("audit.log"),
            token_file: None,
            tls_cert: Some(cert_path),
            tls_key: Some(key_path),
        };

        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let _handle = tokio::spawn(async move {
            tokio::select! {
                _ = run_daemon(cfg) => {},
                _ = rx => {},
            }
        });

        let base = format!("https://127.0.0.1:{port}");
        let client = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .build()
            .unwrap();

        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if client.get(format!("{base}/healthz")).send().await.is_ok() {
                return Self {
                    base,
                    _shutdown: tx,
                    _td: td,
                };
            }
        }
        panic!("TLS daemon never became reachable on {base}");
    }
}

#[tokio::test]
async fn end_to_end_tls_serves_https() {
    let d = TlsTestDaemon::start().await;
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();

    let h = client
        .get(format!("{}/healthz", d.base))
        .send()
        .await
        .unwrap();
    assert_eq!(h.status(), 200);
    let body: Value = h.json().await.unwrap();
    assert_eq!(body["ok"], Value::Bool(true));

    let v: Value = client
        .get(format!("{}/version", d.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["api"], "v1");
}

#[tokio::test]
async fn end_to_end_audit_log_records_request() {
    let d = TestDaemon::start().await;
    let _ = reqwest::get(format!("{}/version", d.base)).await.unwrap();
    // Audit log path is inside the TempDir.
    let audit = d._td.path().join("audit.log");
    // The audit middleware writes after the handler returns; give it
    // a beat to flush.
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if let Ok(contents) = std::fs::read_to_string(&audit) {
            if contents.contains("\"/version\"") {
                assert!(contents.contains("\"method\":\"GET\""));
                assert!(contents.contains("\"status\":200"));
                return;
            }
        }
    }
    panic!("audit log never captured /version request");
}
