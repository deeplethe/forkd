//! `forkd-controller`: per-host daemon that owns the VM lifecycle.
//!
//! Run with:
//!   forkd-controller serve \
//!       --bind 127.0.0.1:8889 \
//!       --state /var/lib/forkd/state.json \
//!       --audit-log /var/log/forkd/audit.log \
//!       --token-file /etc/forkd/token
//!
//! The daemon owns the on-disk snapshot registry, active child
//! Firecracker processes, cgroup parents for quota enforcement, the
//! `/metrics` endpoint, and the append-only audit log. Clients (CLI,
//! Python SDK) talk to it via HTTP/JSON over loopback by default;
//! supply `--token-file` for multi-tenant or networked deployments.
use anyhow::Result;
use clap::{Parser, Subcommand};
use forkd_controller::{run_daemon, DaemonConfig};
use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "forkd-controller", version, about = "forkd controller daemon")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Start the daemon and listen for HTTP requests.
    Serve {
        /// Bind address. Default is loopback only.
        #[arg(long, env = "FORKD_BIND", default_value = "127.0.0.1:8889")]
        bind: SocketAddr,
        /// On-disk JSON state file. Auto-created on first run.
        #[arg(long, env = "FORKD_STATE", default_value = "/var/lib/forkd/state.json")]
        state: PathBuf,
        /// Root directory under which tagged snapshots are stored
        /// (each tag is a subdir with vmstate + memory.bin).
        #[arg(long, env = "FORKD_SNAPSHOT_ROOT")]
        snapshot_root: Option<PathBuf>,
        /// Append-only audit log. One JSON line per request.
        #[arg(
            long,
            env = "FORKD_AUDIT_LOG",
            default_value = "/var/log/forkd/audit.log"
        )]
        audit_log: PathBuf,
        /// Path to a file containing the daemon's bearer token.
        /// When unset, the daemon runs without authentication —
        /// safe only for loopback-bound, single-tenant developer use.
        #[arg(long, env = "FORKD_TOKEN_FILE")]
        token_file: Option<PathBuf>,
        /// PEM-encoded TLS server certificate chain. Required with
        /// --tls-key to enable HTTPS.
        #[arg(long, env = "FORKD_TLS_CERT")]
        tls_cert: Option<PathBuf>,
        /// PEM-encoded TLS private key matching --tls-cert.
        #[arg(long, env = "FORKD_TLS_KEY")]
        tls_key: Option<PathBuf>,
        /// Scratch directory for prewarm throwaway snapshots. Used only
        /// for sandbox-create requests with `"prewarm": true`. tmpfs is
        /// strongly preferred; the directory must hold one guest-RAM
        /// file per concurrent prewarmed child. Default
        /// `/dev/shm/forkd-prewarm`.
        #[arg(
            long,
            env = "FORKD_PREWARM_SCRATCH_DIR",
            default_value = "/dev/shm/forkd-prewarm"
        )]
        prewarm_scratch_dir: PathBuf,
        /// Maximum concurrent BRANCH operations the daemon will admit.
        /// Each BRANCH writes a full memory.bin (typically 256 MiB - 8 GiB),
        /// so this cap bounds peak transient disk usage during fan-outs.
        /// Tune higher on hosts with fast NVMe + small snapshots; tune lower
        /// on hosts with constrained disk. Defaults to 4.
        #[arg(long, env = "FORKD_BRANCH_CONCURRENCY")]
        branch_concurrency: Option<usize>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Serve {
            bind,
            state,
            snapshot_root,
            audit_log,
            token_file,
            tls_cert,
            tls_key,
            prewarm_scratch_dir,
            branch_concurrency,
        } => {
            let defaults = DaemonConfig::default();
            run_daemon(DaemonConfig {
                bind,
                state_file: state,
                snapshot_root: snapshot_root.unwrap_or(defaults.snapshot_root),
                audit_log,
                token_file,
                tls_cert,
                tls_key,
                prewarm_scratch_dir,
                branch_concurrency,
            })
            .await
        }
    }
}
