//! `forkd ls` + `forkd kill` — direct sandbox lifecycle without curl.
//!
//! Wraps the two endpoints (GET /v1/sandboxes, DELETE /v1/sandboxes/:id)
//! that previously required hand-written curl invocations. Output is
//! a formatted table for `ls` and a per-id status line for `kill`.

use anyhow::{Context, Result};
use std::time::Duration;

/// `forkd ls` — list live sandboxes the daemon knows about.
pub fn ls(daemon_url: &str, token: Option<String>) -> Result<()> {
    let sandboxes = list_sandboxes(daemon_url, token.as_deref())?;
    if sandboxes.is_empty() {
        eprintln!("no live sandboxes");
        return Ok(());
    }
    // Column widths.
    let id_w = sandboxes
        .iter()
        .filter_map(|s| s.get("id").and_then(|v| v.as_str()))
        .map(str::len)
        .max()
        .unwrap_or(8)
        .max(8);
    let tag_w = sandboxes
        .iter()
        .filter_map(|s| s.get("snapshot_tag").and_then(|v| v.as_str()))
        .map(str::len)
        .max()
        .unwrap_or(8)
        .max(8);
    println!(
        "  {:<id_w$}  {:<tag_w$}  {:<8}  {:<14}  {:<8}  GUEST_ADDR",
        "ID",
        "SNAPSHOT",
        "PID",
        "NETNS",
        "BRANCHES",
        id_w = id_w,
        tag_w = tag_w,
    );
    for s in &sandboxes {
        let id = s.get("id").and_then(|v| v.as_str()).unwrap_or("?");
        let tag = s
            .get("snapshot_tag")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let pid = s
            .get("pid")
            .and_then(|v| v.as_u64())
            .map(|p| p.to_string())
            .unwrap_or_else(|| "—".to_string());
        let netns = s.get("netns").and_then(|v| v.as_str()).unwrap_or("—");
        let guest = s.get("guest_addr").and_then(|v| v.as_str()).unwrap_or("—");
        // branch_count is informational (the v0.3 multi-BRANCH pause
        // anomaly that originally motivated the warning was fixed in
        // v0.3.4 via posix_fallocate; see #146).
        let bc = s
            .get("branch_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            .to_string();
        println!(
            "  {:<id_w$}  {:<tag_w$}  {:<8}  {:<14}  {:<8}  {}",
            id,
            tag,
            pid,
            netns,
            bc,
            guest,
            id_w = id_w,
            tag_w = tag_w,
        );
    }
    println!(
        "\n  {} sandbox{}",
        sandboxes.len(),
        if sandboxes.len() == 1 { "" } else { "es" }
    );
    Ok(())
}

/// `forkd kill` — terminate one or more sandboxes via DELETE.
pub fn kill(
    daemon_url: &str,
    token: Option<String>,
    ids: Vec<String>,
    all: bool,
    tag: Option<String>,
) -> Result<()> {
    let targets: Vec<String> = if all || tag.is_some() {
        let sandboxes = list_sandboxes(daemon_url, token.as_deref())?;
        sandboxes
            .iter()
            .filter(|s| match &tag {
                Some(t) => s
                    .get("snapshot_tag")
                    .and_then(|v| v.as_str())
                    .map(|x| x == t)
                    .unwrap_or(false),
                None => true,
            })
            .filter_map(|s| s.get("id").and_then(|v| v.as_str()).map(String::from))
            .collect()
    } else {
        if ids.is_empty() {
            anyhow::bail!("no sandbox specified; pass <ID>... or --all or --tag <TAG>");
        }
        ids
    };

    if targets.is_empty() {
        eprintln!("no matching sandboxes");
        return Ok(());
    }

    let mut errs = 0;
    for id in &targets {
        match delete_sandbox(daemon_url, token.as_deref(), id) {
            Ok(()) => println!("  ✓ {id}"),
            Err(e) => {
                println!("  ✗ {id}  ({e})");
                errs += 1;
            }
        }
    }
    if errs > 0 {
        anyhow::bail!("{errs} of {} kills failed", targets.len());
    }
    Ok(())
}

// ----------------------------------------------------------------------
// HTTP helpers
// ----------------------------------------------------------------------

fn list_sandboxes(daemon_url: &str, token: Option<&str>) -> Result<Vec<serde_json::Value>> {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(10))
        .build();
    let url = format!("{}/v1/sandboxes", daemon_url.trim_end_matches('/'));
    let mut req = agent.get(&url);
    if let Some(t) = token {
        req = req.set("Authorization", &format!("Bearer {t}"));
    }
    let resp = req.call().map_err(map_err)?;
    let body = resp.into_string().context("read body")?;
    let v: serde_json::Value =
        serde_json::from_str(&body).with_context(|| format!("parse JSON: {body}"))?;
    Ok(v.as_array().cloned().unwrap_or_default())
}

fn delete_sandbox(daemon_url: &str, token: Option<&str>, id: &str) -> Result<()> {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(30))
        .build();
    let url = format!("{}/v1/sandboxes/{}", daemon_url.trim_end_matches('/'), id);
    let mut req = agent.delete(&url);
    if let Some(t) = token {
        req = req.set("Authorization", &format!("Bearer {t}"));
    }
    req.call().map_err(map_err)?;
    Ok(())
}

fn map_err(e: ureq::Error) -> anyhow::Error {
    match e {
        ureq::Error::Status(code, r) => {
            let body = r.into_string().unwrap_or_default();
            anyhow::anyhow!("HTTP {code}: {body}")
        }
        e => anyhow::anyhow!("transport: {e}"),
    }
}
