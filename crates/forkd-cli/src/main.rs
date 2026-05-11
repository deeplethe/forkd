//! `forkd` — the CLI entrypoint.

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "forkd",
    version,
    about = "Fork microVMs the way you fork processes."
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Snapshot a warm parent VM.
    Snapshot {
        #[arg(long)]
        tag: String,
    },
    /// Fork N children from a snapshot.
    Fork {
        #[arg(long)]
        tag: String,
        #[arg(long, short)]
        n: usize,
        /// Command to run in each child (everything after `--`).
        #[arg(last = true)]
        cmd: Vec<String>,
    },
    /// List active VMs.
    List,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();
    match cli.command {
        Cmd::Snapshot { tag } => {
            anyhow::bail!("not yet implemented: snapshot {tag}");
        }
        Cmd::Fork { tag, n, cmd } => {
            anyhow::bail!("not yet implemented: fork {tag} n={n} cmd={cmd:?}");
        }
        Cmd::List => {
            anyhow::bail!("not yet implemented: list");
        }
    }
}
