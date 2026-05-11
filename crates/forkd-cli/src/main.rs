//! `forkd` — CLI entrypoint.
//!
//! Subcommands:
//!   forkd snapshot --tag <name> --kernel <path> --rootfs <path>
//!   forkd fork --tag <name> --n <N>
//!
//! Snapshots live under $XDG_DATA_HOME/forkd/snapshots/<tag>/.

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use forkd_vmm::{
    eval_at, eval_in_netns, exec_at, exec_in_netns, ping_at, ping_in_netns, BootConfig, ForkOpts,
    NetworkConfig, Snapshot, Vm,
};
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

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
    /// Boot a parent VM, warm it up, snapshot to disk.
    Snapshot {
        /// Name of the snapshot. Becomes ~/.local/share/forkd/snapshots/<tag>/.
        #[arg(long)]
        tag: String,
        /// Path to vmlinux kernel.
        #[arg(long, env = "FORKD_KERNEL")]
        kernel: PathBuf,
        /// Path to rootfs image. Pass `.ext4` for read-write, or `.squashfs` for read-only.
        #[arg(long, env = "FORKD_ROOTFS")]
        rootfs: PathBuf,
        /// Mount rootfs read-write (auto-enabled for `*.ext4`).
        #[arg(long)]
        rw: bool,
        /// Host tap device name to attach as the guest's eth0.
        /// Create with `scripts/day4-network.sh` (e.g. forkd-tap0).
        #[arg(long, env = "FORKD_TAP")]
        tap: Option<String>,
        /// Seconds to wait for guest to settle before snapshotting.
        #[arg(long, default_value_t = 10)]
        boot_wait_secs: u64,
    },
    /// Fork N children from a tagged snapshot.
    Fork {
        #[arg(long)]
        tag: String,
        #[arg(long, short)]
        n: usize,
        /// Seconds to let children run before reporting / shutting down.
        #[arg(long, default_value_t = 2)]
        settle_secs: u64,
        /// Spawn each child inside its own `forkd-child-<i>` netns.
        /// Run `sudo scripts/netns-setup.sh N` first.
        #[arg(long)]
        per_child_netns: bool,
    },
    /// Run a command inside a forked child via the guest agent.
    ///
    /// Example: forkd exec -- python3 -c "import numpy; print(numpy.zeros(3))"
    Exec {
        /// Address of the guest agent. Default matches NetworkConfig::default_tap().
        #[arg(long, default_value = "10.42.0.2:8888")]
        target: String,
        /// Net namespace to enter (e.g. `forkd-child-3`). Requires root.
        #[arg(long)]
        child: Option<String>,
        /// Command timeout in seconds.
        #[arg(long, default_value_t = 30)]
        timeout_secs: u64,
        /// Command and args (everything after `--`).
        #[arg(last = true)]
        cmd: Vec<String>,
    },
    /// Ping the guest agent to verify it's up.
    Ping {
        #[arg(long, default_value = "10.42.0.2:8888")]
        target: String,
        #[arg(long)]
        child: Option<String>,
    },
    /// Evaluate a Python expression against the warmed PID-1 interpreter.
    ///
    /// Example: forkd eval -- "numpy.zeros(3).sum()"
    Eval {
        #[arg(long, default_value = "10.42.0.2:8888")]
        target: String,
        #[arg(long)]
        child: Option<String>,
        /// Python expression to evaluate (everything after `--`).
        #[arg(last = true)]
        code: Vec<String>,
    },
    /// Show where snapshots are stored.
    Where,
}

fn data_dir() -> PathBuf {
    if let Ok(d) = std::env::var("XDG_DATA_HOME") {
        return PathBuf::from(d).join("forkd");
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".local/share/forkd")
}

fn snapshot_dir(tag: &str) -> PathBuf {
    data_dir().join("snapshots").join(tag)
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();
    match cli.command {
        Cmd::Snapshot {
            tag,
            kernel,
            rootfs,
            rw,
            tap,
            boot_wait_secs,
        } => snapshot_cmd(tag, kernel, rootfs, rw, tap, boot_wait_secs),
        Cmd::Fork {
            tag,
            n,
            settle_secs,
            per_child_netns,
        } => fork_cmd(tag, n, settle_secs, per_child_netns),
        Cmd::Exec {
            target,
            child,
            timeout_secs,
            cmd,
        } => exec_cmd(target, child, timeout_secs, cmd),
        Cmd::Ping { target, child } => ping_cmd(target, child),
        Cmd::Eval {
            target,
            child,
            code,
        } => eval_cmd(target, child, code),
        Cmd::Where => {
            println!("{}", data_dir().display());
            Ok(())
        }
    }
}

fn exec_cmd(
    target: String,
    child: Option<String>,
    timeout_secs: u64,
    cmd: Vec<String>,
) -> Result<()> {
    if cmd.is_empty() {
        bail!("no command provided. Usage: forkd exec -- <cmd> [args...]");
    }
    let timeout = Duration::from_secs(timeout_secs);
    let resp = match child {
        Some(ns) => exec_in_netns(&ns, target, cmd, timeout)?,
        None => exec_at(&target, cmd, timeout)?,
    };
    if !resp.stdout.is_empty() {
        print!("{}", resp.stdout);
    }
    if !resp.stderr.is_empty() {
        eprint!("{}", resp.stderr);
    }
    if let Some(err) = resp.error {
        bail!("agent error: {err}");
    }
    std::process::exit(resp.exit_code);
}

fn ping_cmd(target: String, child: Option<String>) -> Result<()> {
    let pong = match child {
        Some(ns) => ping_in_netns(&ns, target)?,
        None => ping_at(&target)?,
    };
    println!("{}", serde_json::to_string_pretty(&pong)?);
    Ok(())
}

fn eval_cmd(target: String, child: Option<String>, code: Vec<String>) -> Result<()> {
    if code.is_empty() {
        bail!("no expression provided. Usage: forkd eval -- <python expr>");
    }
    let expr = code.join(" ");
    let v = match child {
        Some(ns) => eval_in_netns(&ns, target, expr)?,
        None => eval_at(&target, expr)?,
    };
    if let Some(err) = v.get("error").and_then(|e| e.as_str()) {
        eprintln!("error: {err}");
        if let Some(tb) = v.get("traceback").and_then(|t| t.as_str()) {
            eprintln!("{tb}");
        }
        std::process::exit(1);
    }
    if let Some(r) = v.get("result").and_then(|r| r.as_str()) {
        println!("{r}");
    }
    Ok(())
}

fn snapshot_cmd(
    tag: String,
    kernel: PathBuf,
    rootfs: PathBuf,
    rw_flag: bool,
    tap: Option<String>,
    boot_wait_secs: u64,
) -> Result<()> {
    if !kernel.exists() {
        bail!("kernel not found: {}", kernel.display());
    }
    if !rootfs.exists() {
        bail!("rootfs not found: {}", rootfs.display());
    }

    // Auto-detect ext4 by extension; or explicit --rw flag.
    let rw = rw_flag
        || rootfs
            .extension()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s == "ext4");

    let work_dir = std::env::temp_dir().join(format!("forkd-parent-{tag}"));
    let mut cfg = if rw {
        eprintln!("    rootfs mode: read-write (ext4)");
        BootConfig::ext4_rw(kernel, rootfs, work_dir.clone())
    } else {
        eprintln!("    rootfs mode: read-only (squashfs)");
        BootConfig::quickstart(kernel, rootfs, work_dir.clone())
    };

    if let Some(tap_name) = tap {
        let net = NetworkConfig::default_tap(&tap_name);
        eprintln!(
            "    network: virtio-net via tap {} (guest {} ↔ host {})",
            tap_name,
            net.guest_ip.as_deref().unwrap_or("?"),
            net.host_gw.as_deref().unwrap_or("?")
        );
        cfg = cfg.with_network(net);
    }

    eprintln!("==> booting parent VM (work_dir={})...", work_dir.display());
    let mut vm = Vm::boot(&cfg).context("boot parent")?;
    eprintln!("    firecracker pid: {}", vm.pid());

    eprintln!("==> warming up for {boot_wait_secs}s...");
    thread::sleep(Duration::from_secs(boot_wait_secs));

    eprintln!("==> pausing...");
    vm.pause().context("pause parent")?;

    let snap_dir = snapshot_dir(&tag);
    std::fs::create_dir_all(&snap_dir).context("create snapshot dir")?;
    let vmstate = snap_dir.join("vmstate");
    let memory = snap_dir.join("memory.bin");

    eprintln!("==> snapshotting to {}...", snap_dir.display());
    let t = Instant::now();
    vm.snapshot_to(vmstate, memory).context("snapshot create")?;
    eprintln!("    snapshot took {} ms", t.elapsed().as_millis());

    vm.kill().context("kill parent")?;
    eprintln!("✓ tag '{tag}' ready. Try: forkd fork --tag {tag} --n 10");
    Ok(())
}

fn fork_cmd(tag: String, n: usize, settle_secs: u64, per_child_netns: bool) -> Result<()> {
    let snap_dir = snapshot_dir(&tag);
    let vmstate = snap_dir.join("vmstate");
    let memory = snap_dir.join("memory.bin");

    if !vmstate.exists() {
        bail!(
            "snapshot tag '{tag}' not found at {}\n\
             run 'forkd snapshot --tag {tag} ...' first",
            snap_dir.display()
        );
    }

    let snapshot = Snapshot { vmstate, memory };
    let work_dir = std::env::temp_dir().join(format!("forkd-fork-{tag}"));

    eprintln!(
        "==> forking {n} children from snapshot '{tag}'{}...",
        if per_child_netns {
            " (per-child netns)"
        } else {
            ""
        }
    );
    let result = snapshot
        .restore_many_with(
            ForkOpts {
                n,
                per_child_netns,
            },
            &work_dir,
        )
        .context("restore_many_with failed")?;

    let total_ms = result.spawn_ms + result.restore_ms;
    println!("✓ all sockets up in {} ms", result.spawn_ms);
    println!(
        "✓ {n} restores fired in parallel in {} ms",
        result.restore_ms
    );
    println!("✓ total wall-clock: {total_ms} ms");

    eprintln!("==> letting children settle for {settle_secs}s...");
    thread::sleep(Duration::from_secs(settle_secs));

    let alive = result.children.iter().filter(|c| c.is_alive()).count();
    println!("✓ {alive} / {n} children alive");

    eprintln!("==> shutting down...");
    for c in &result.children {
        let _ = c.shutdown();
    }
    thread::sleep(Duration::from_secs(2));
    drop(result); // triggers kill via Drop for any still alive

    Ok(())
}
