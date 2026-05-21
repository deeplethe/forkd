//! `forkd bench` — quick latency probe against a live daemon.
//!
//! Runs a representative spawn → exec → branch → fanout → cleanup
//! cycle and prints per-step timing. The point is to answer
//! "is forkd actually fast on YOUR box?" without making the user
//! cook up a benchmark themselves. Output is screenshot-friendly.

use anyhow::{Context, Result};
use std::time::{Duration, Instant};

pub fn run(
    daemon_url: &str,
    daemon_token: Option<String>,
    tag: Option<String>,
    fanout_n: usize,
    netns: bool,
) -> Result<()> {
    let client = Client::new(daemon_url, daemon_token);

    // 1) Pick a snapshot.
    let tag = match tag {
        Some(t) => t,
        None => {
            let snaps = client.list_snapshots()?;
            let first = snaps
                .iter()
                .filter_map(|v| v.get("tag").and_then(|t| t.as_str()))
                .next()
                .ok_or_else(|| {
                    anyhow::anyhow!("no snapshots on the daemon; build one with `forkd snapshot`")
                })?;
            first.to_string()
        }
    };
    println!("forkd bench against snapshot \x1b[1m{tag}\x1b[0m");
    println!("  fanout n={fanout_n} per_child_netns={netns}\n");

    let total_start = Instant::now();

    // 2) Spawn 1 source sandbox.
    let t = Instant::now();
    let source = client.spawn_one(&tag)?;
    let spawn_ms = t.elapsed().as_millis();
    let source_id = source
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("spawn response missing id: {source}"))?
        .to_string();
    print_row("spawn (n=1)", spawn_ms, &source_id);

    // 3) Exec round-trip (sh -c echo).
    let t = Instant::now();
    let exec = client.exec(&source_id, &["sh", "-c", "echo bench"])?;
    let exec_ms = t.elapsed().as_millis();
    let exit_code = exec.get("exit_code").and_then(|v| v.as_i64()).unwrap_or(-1);
    print_row("exec round-trip", exec_ms, &format!("exit={exit_code}"));

    // 4) Diff BRANCH.
    let t = Instant::now();
    let branch = client.branch_diff(&source_id)?;
    let branch_client_ms = t.elapsed().as_millis();
    let branch_tag = branch
        .get("tag")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("branch response missing tag: {branch}"))?
        .to_string();
    let pause_ms = branch.get("pause_ms").and_then(|v| v.as_u64()).unwrap_or(0);
    let diff_bytes = branch
        .get("diff_physical_bytes")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    print_row(
        "branch (diff=true)",
        branch_client_ms,
        &format!("pause_ms={pause_ms} diff_physical_bytes={diff_bytes}"),
    );

    // 5) Fanout N grandchildren from the branch.
    let t = Instant::now();
    let kids = client.spawn_many(&branch_tag, fanout_n, netns)?;
    let fanout_ms = t.elapsed().as_millis();
    let per_child = if fanout_n > 0 {
        fanout_ms / fanout_n as u128
    } else {
        0
    };
    print_row(
        &format!("fanout (n={fanout_n})"),
        fanout_ms,
        &format!("{per_child}ms/child"),
    );

    // 6) Cleanup.
    let t = Instant::now();
    let kid_ids: Vec<String> = kids
        .iter()
        .filter_map(|k| k.get("id").and_then(|v| v.as_str()).map(String::from))
        .collect();
    for k in &kid_ids {
        let _ = client.kill(k);
    }
    let _ = client.kill(&source_id);
    let cleanup_ms = t.elapsed().as_millis();
    print_row(
        "cleanup",
        cleanup_ms,
        &format!("{} sandboxes", kid_ids.len() + 1),
    );

    let total_ms = total_start.elapsed().as_millis();
    println!("                          -----");
    println!("  \x1b[1m{:<22}{:>5} ms\x1b[0m", "total", total_ms);
    Ok(())
}

fn print_row(name: &str, ms: u128, detail: &str) {
    println!("  {:<22}{:>5} ms  \x1b[90m{}\x1b[0m", name, ms, detail);
}

// ----------------------------------------------------------------------
// HTTP client — small wrapper around ureq for the few endpoints we need.
// Avoids pulling reqwest just for the bench command.
// ----------------------------------------------------------------------

struct Client {
    agent: ureq::Agent,
    base: String,
    token: Option<String>,
}

impl Client {
    fn new(base: &str, token: Option<String>) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(60))
            .build();
        Self {
            agent,
            base: base.trim_end_matches('/').to_string(),
            token,
        }
    }

    fn req(&self, method: &str, path: &str) -> ureq::Request {
        let mut r = self.agent.request(method, &format!("{}{path}", self.base));
        if let Some(t) = &self.token {
            r = r.set("Authorization", &format!("Bearer {t}"));
        }
        r.set("Content-Type", "application/json")
    }

    fn list_snapshots(&self) -> Result<Vec<serde_json::Value>> {
        let resp = self
            .req("GET", "/v1/snapshots")
            .call()
            .map_err(map_ureq_err)?;
        let v: serde_json::Value = parse_json_resp(resp).context("parse snapshots")?;
        Ok(v.as_array().cloned().unwrap_or_default())
    }

    fn spawn_one(&self, tag: &str) -> Result<serde_json::Value> {
        let body = serde_json::json!({"snapshot_tag": tag, "n": 1});
        let resp = self
            .req("POST", "/v1/sandboxes")
            .send_string(&body.to_string())
            .map_err(map_ureq_err)?;
        let v: serde_json::Value = parse_json_resp(resp).context("parse spawn")?;
        v.as_array()
            .and_then(|a| a.first().cloned())
            .ok_or_else(|| anyhow::anyhow!("spawn returned empty array: {v}"))
    }

    fn spawn_many(
        &self,
        tag: &str,
        n: usize,
        per_child_netns: bool,
    ) -> Result<Vec<serde_json::Value>> {
        let body = serde_json::json!({
            "snapshot_tag": tag,
            "n": n,
            "per_child_netns": per_child_netns
        });
        let resp = self
            .req("POST", "/v1/sandboxes")
            .send_string(&body.to_string())
            .map_err(map_ureq_err)?;
        let v: serde_json::Value = parse_json_resp(resp).context("parse spawn_many")?;
        Ok(v.as_array().cloned().unwrap_or_default())
    }

    fn exec(&self, id: &str, args: &[&str]) -> Result<serde_json::Value> {
        let body = serde_json::json!({"args": args, "timeout_secs": 5});
        let resp = self
            .req("POST", &format!("/v1/sandboxes/{id}/exec"))
            .send_string(&body.to_string())
            .map_err(map_ureq_err)?;
        parse_json_resp(resp).context("parse exec")
    }

    fn branch_diff(&self, id: &str) -> Result<serde_json::Value> {
        let body = serde_json::json!({"diff": true});
        let resp = self
            .req("POST", &format!("/v1/sandboxes/{id}/branch"))
            .send_string(&body.to_string())
            .map_err(map_ureq_err)?;
        parse_json_resp(resp).context("parse branch")
    }

    fn kill(&self, id: &str) -> Result<()> {
        self.req("DELETE", &format!("/v1/sandboxes/{id}"))
            .call()
            .map_err(map_ureq_err)?;
        Ok(())
    }
}

fn parse_json_resp(resp: ureq::Response) -> Result<serde_json::Value> {
    // ureq 2.x is built without the `json` feature here; parse the
    // body string ourselves.
    let body = resp.into_string().context("read response body")?;
    serde_json::from_str(&body).with_context(|| format!("parse JSON: {body}"))
}

fn map_ureq_err(e: ureq::Error) -> anyhow::Error {
    match e {
        ureq::Error::Status(code, r) => {
            let body = r.into_string().unwrap_or_default();
            anyhow::anyhow!("daemon HTTP {code}: {body}")
        }
        e => anyhow::anyhow!("daemon transport: {e}"),
    }
}
