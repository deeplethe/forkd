//! `forkd` — one binary, two modes.

use anyhow::{bail, Result};
use clap::{Args, Parser, Subcommand};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use forkd_core::identity::random_token;
use forkd_core::master::{self, MasterConfig};
use forkd_core::node::{self, NodeConfig, RuntimeConfig};
use forkd_core::state::Store;
use forkd_core::tier::IdlePolicy;

#[derive(Parser)]
#[command(name = "forkd", version, about = "sandbox fleet, oversold on purpose")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the master, a node, or both in one process.
    Serve(Box<ServeArgs>),
    /// Mint a single-use registration token for a new node.
    NodeToken(NodeTokenArgs),
    /// List registered nodes.
    Nodes(StateArgs),
}

#[derive(Args)]
struct ServeArgs {
    /// Run the control plane here.
    #[arg(long)]
    master: bool,
    /// Run the sandbox agent here.
    #[arg(long)]
    node: bool,

    #[arg(long, default_value = "127.0.0.1:8080", env = "FORKD_BIND")]
    bind: SocketAddr,
    #[arg(
        long,
        default_value = "/var/lib/forkd/state.db",
        env = "FORKD_STATE_DB"
    )]
    state_db: PathBuf,
    #[arg(long, default_value = "/var/lib/forkd/pki", env = "FORKD_PKI_DIR")]
    pki_dir: PathBuf,

    /// Master to dial. Defaults to this process's own bind address when
    /// --master is also set, which is what makes the single-machine case
    /// a one-flag deployment.
    #[arg(long, env = "FORKD_MASTER_URL")]
    master_url: Option<String>,
    #[arg(long, env = "FORKD_NODE_ID")]
    node_id: Option<String>,
    /// Registration token. Not needed once this node holds a certificate,
    /// and not needed at all when --master runs in the same process.
    #[arg(long, env = "FORKD_NODE_TOKEN")]
    node_token: Option<String>,

    #[arg(long, default_value = "/var/lib/forkd", env = "FORKD_WORK_DIR")]
    work_dir: PathBuf,
    #[arg(long, default_value = "runsc")]
    runsc: String,
    /// runsc's state directory. Per-deployment by default: sharing one
    /// means two forkd nodes would each see the other's containers as
    /// orphans and destroy them.
    #[arg(long, default_value = "/var/run/forkd-runsc")]
    runsc_root: PathBuf,
    /// Shared volume root. A JuiceFS mount in a fleet; local on one box.
    #[arg(long, env = "FORKD_VOLUME_ROOT")]
    volume_root: Option<PathBuf>,
    /// 0 means "as many as memory allows".
    #[arg(long, default_value_t = 0)]
    max_sandboxes: u32,

    #[arg(long, default_value_t = 30)]
    idle_frozen_secs: u64,
    #[arg(long, default_value_t = 300)]
    idle_snapshot_secs: u64,
    #[arg(long, default_value_t = 3600)]
    idle_detached_secs: u64,
    /// How long the master parks a node's request when it has no work.
    /// Not a scheduling delay -- commands arrive on the parked request.
    #[arg(long, default_value_t = 25)]
    hold_secs: u64,
}

#[derive(Args)]
struct NodeTokenArgs {
    #[arg(long, default_value = "default")]
    pool: String,
    /// Short by design: a bootstrap credential should not outlive the
    /// terminal session that created it.
    #[arg(long, default_value_t = 900)]
    ttl_secs: i64,
    #[arg(
        long,
        default_value = "/var/lib/forkd/state.db",
        env = "FORKD_STATE_DB"
    )]
    state_db: PathBuf,
}

#[derive(Args)]
struct StateArgs {
    #[arg(
        long,
        default_value = "/var/lib/forkd/state.db",
        env = "FORKD_STATE_DB"
    )]
    state_db: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    match Cli::parse().cmd {
        Cmd::Serve(a) => serve(*a).await,
        Cmd::NodeToken(a) => {
            let store = Store::open(&a.state_db)?;
            let token = random_token();
            store.create_node_token(&token, &a.pool, a.ttl_secs)?;
            println!("{token}");
            eprintln!("pool={} valid for {}s, single use", a.pool, a.ttl_secs);
            Ok(())
        }
        Cmd::Nodes(a) => {
            let store = Store::open(&a.state_db)?;
            println!(
                "{:<20} {:<12} {:<18} {:<6} STATE",
                "ID", "POOL", "FINGERPRINT", "MAX"
            );
            for n in store.list_nodes()? {
                println!(
                    "{:<20} {:<12} {:<18} {:<6} {}",
                    n.id,
                    n.pool,
                    n.fingerprint,
                    n.max_sandboxes,
                    if n.connected { "connected" } else { "offline" }
                );
            }
            Ok(())
        }
    }
}

async fn serve(a: ServeArgs) -> Result<()> {
    if !a.master && !a.node {
        bail!("pass --master, --node, or both");
    }

    let idle = IdlePolicy {
        to_frozen: Duration::from_secs(a.idle_frozen_secs),
        to_snapshot: Duration::from_secs(a.idle_snapshot_secs),
        to_detached: Duration::from_secs(a.idle_detached_secs),
    };

    let master_fut = if a.master {
        let cfg = MasterConfig {
            bind: a.bind,
            state_db: a.state_db.clone(),
            pki_dir: a.pki_dir.clone(),
            idle,
        };
        Some(tokio::spawn(master::serve(cfg)))
    } else {
        None
    };

    let node_fut = if a.node {
        let master_url = match a.master_url.clone() {
            Some(u) => u,
            None if a.master => format!("http://{}", a.bind),
            None => bail!("--node without --master needs --master-url"),
        };
        let node_id = a
            .node_id
            .clone()
            .or_else(hostname)
            .unwrap_or_else(|| format!("node-{}", &random_token()[..8]));

        // In-process master means we can mint our own registration token
        // rather than making the operator copy one between two terminals.
        let token = match a.node_token.clone() {
            Some(t) => Some(t),
            None if a.master => {
                // Give the master a moment to create the DB.
                tokio::time::sleep(Duration::from_millis(300)).await;
                let store = Store::open(&a.state_db)?;
                let t = random_token();
                store.create_node_token(&t, "default", 300)?;
                Some(t)
            }
            None => None,
        };

        let cfg = NodeConfig {
            node_id,
            master_url,
            token,
            pki_dir: a.pki_dir.join("node"),
            runtime: RuntimeConfig {
                runsc: a.runsc.clone(),
                runsc_root: a.runsc_root.clone(),
                work_dir: a.work_dir.clone(),
                max_sandboxes: a.max_sandboxes,
                volume_root: a
                    .volume_root
                    .clone()
                    .unwrap_or_else(|| a.work_dir.join("volumes")),
            },
            hold: Duration::from_secs(a.hold_secs),
        };
        Some(tokio::spawn(node::serve(cfg)))
    } else {
        None
    };

    // Either half failing takes the process down; systemd restarts it, and
    // the sandboxes do not care because they live in their own scopes.
    match (master_fut, node_fut) {
        (Some(m), Some(n)) => tokio::select! {
            r = m => r??,
            r = n => r??,
        },
        (Some(m), None) => m.await??,
        (None, Some(n)) => n.await??,
        (None, None) => unreachable!("checked above"),
    }
    Ok(())
}

fn hostname() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}
