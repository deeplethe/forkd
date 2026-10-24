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
#[command(
    name = "forkd-serve",
    version,
    about = "sandbox fleet, oversold on purpose"
)]
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

    /// Range sandbox ports are allocated from, per node.
    #[arg(long, default_value_t = 20000)]
    port_min: u16,
    #[arg(long, default_value_t = 32000)]
    port_max: u16,

    /// Bearer token the API requires. Read from this file; one is generated
    /// on first run if the file does not exist.
    ///
    /// Not optional in practice: code inside a sandbox can reach the master
    /// over loopback, so an open control plane lets a sandbox mint node
    /// tokens and delete other sandboxes. `--api-open` is the explicit
    /// opt-out for a throwaway single-node run.
    #[arg(long, env = "FORKD_API_TOKEN_FILE")]
    api_token_file: Option<PathBuf>,
    /// Run the API with no authentication. Only for a node nothing else can
    /// reach, and never with untrusted sandboxes on it.
    #[arg(long)]
    api_open: bool,
    /// Serve the control plane as plain HTTP instead of TLS.
    ///
    /// TLS is the default because the control plane carries registration
    /// tokens, the API token and every command run inside a sandbox. Without
    /// it the master also cannot tell one node from another -- a bearer
    /// token is one shared secret, so any holder could poll as any node --
    /// so `--insecure-transport` turns the client-certificate check off too.
    #[arg(long)]
    insecure_transport: bool,

    /// Range sandbox network namespaces are addressed from. Each sandbox
    /// gets a /30 out of it, so a /16 holds 16384 of them.
    #[arg(long, default_value = "10.86.0.0")]
    net_base: std::net::Ipv4Addr,
    #[arg(long, default_value_t = 16)]
    net_prefix: u8,

    /// Stop placing sandboxes on a node with less free memory than this.
    #[arg(long, default_value_t = 512)]
    admit_min_free_mib: u64,
    /// Stop placing sandboxes on a node whose 10-second memory-pressure
    /// average is above this. On bm, 10 was already degrading and 60 took
    /// the machine off the network.
    #[arg(long, default_value_t = 8.0)]
    admit_max_psi: f64,
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

    // Minted before either half starts, so the node in this same process
    // can read it too.
    let api_token = if a.api_open {
        tracing::warn!(
            "--api-open: the API requires no token. A sandbox on this node can \
             reach it and mint node tokens or delete sandboxes."
        );
        None
    } else {
        let path = a
            .api_token_file
            .clone()
            .unwrap_or_else(|| a.pki_dir.join("api-token"));
        Some(load_or_create_token(&path)?)
    };

    let master_fut = if a.master {
        let cfg = MasterConfig {
            bind: a.bind,
            state_db: a.state_db.clone(),
            pki_dir: a.pki_dir.clone(),
            idle,
            api_token: api_token.clone(),
            // A /30 each, minus the one slot that would run past the end.
            max_net_index: (1u32 << (32 - a.net_prefix.min(30) as u32)) / 4 - 1,
            port_range: (a.port_min, a.port_max),
            insecure_transport: a.insecure_transport,
            admit: forkd_core::master::Admission {
                min_free_mib: a.admit_min_free_mib,
                max_psi_avg10: a.admit_max_psi,
            },
        };
        Some(tokio::spawn(master::serve(cfg)))
    } else {
        None
    };

    let node_fut = if a.node {
        let master_url = match a.master_url.clone() {
            Some(u) => u,
            None if a.master => {
                let scheme = if a.insecure_transport {
                    "http"
                } else {
                    "https"
                };
                format!("{scheme}://{}", a.bind)
            }
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
            api_token: api_token.clone(),
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
                net: forkd_core::node::NetConfig {
                    base: a.net_base,
                    prefix: a.net_prefix,
                },
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

/// Read the API token, creating it on first run.
///
/// Generated rather than required so a first run is still one command, and
/// written at 0600 with its path logged so the operator can find it.
fn load_or_create_token(path: &std::path::Path) -> Result<String> {
    if let Ok(t) = std::fs::read_to_string(path) {
        let t = t.trim().to_string();
        if !t.is_empty() {
            return Ok(t);
        }
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let token = random_token();
    std::fs::write(path, &token)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    tracing::info!(path = %path.display(), "generated an API token");
    Ok(token)
}

fn hostname() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}
