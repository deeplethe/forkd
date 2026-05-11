//! `forkd` — CLI entrypoint.
//!
//! Subcommands:
//!   forkd snapshot --tag <name> --kernel <path> --rootfs <path>
//!   forkd fork --tag <name> --n <N>
//!
//! Snapshots live under $XDG_DATA_HOME/forkd/snapshots/<tag>/.

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use forkd_vmm::{BootConfig, Snapshot, Vm};
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
            boot_wait_secs,
        } => snapshot_cmd(tag, kernel, rootfs, rw, boot_wait_secs),
        Cmd::Fork {
            tag,
            n,
            settle_secs,
        } => fork_cmd(tag, n, settle_secs),
        Cmd::Where => {
            println!("{}", data_dir().display());
            Ok(())
        }
    }
}

fn snapshot_cmd(
    tag: String,
    kernel: PathBuf,
    rootfs: PathBuf,
    rw_flag: bool,
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
    let cfg = if rw {
        eprintln!("    rootfs mode: read-write (ext4)");
        BootConfig::ext4_rw(kernel, rootfs, work_dir.clone())
    } else {
        eprintln!("    rootfs mode: read-only (squashfs)");
        BootConfig::quickstart(kernel, rootfs, work_dir.clone())
    };

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

fn fork_cmd(tag: String, n: usize, settle_secs: u64) -> Result<()> {
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

    eprintln!("==> forking {n} children from snapshot '{tag}'...");
    let result = snapshot
        .restore_many(n, &work_dir)
        .context("restore_many failed")?;

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
