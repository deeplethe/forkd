//! `forkd` — CLI entrypoint.
//!
//! Subcommands:
//!   forkd snapshot --tag <name> --kernel <path> --rootfs <path>
//!   forkd fork --tag <name> --n <N>
//!   forkd pack --tag <name> [--out <file>]    (Snapshot Hub)
//!   forkd unpack <file> [--tag <name>]        (Snapshot Hub)
//!   forkd pull <url> [--tag <name>]           (Snapshot Hub)
//!   forkd images                              (Snapshot Hub)
//!
//! Snapshots live under $XDG_DATA_HOME/forkd/snapshots/<tag>/.

mod hub;

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
        /// Create with `scripts/host-tap.sh` (e.g. forkd-tap0).
        #[arg(long, env = "FORKD_TAP")]
        tap: Option<String>,
        /// Seconds to wait for guest to settle before snapshotting.
        #[arg(long, default_value_t = 10)]
        boot_wait_secs: u64,
        /// Persistent volume to attach to every child of this snapshot.
        /// Format: HOST_FILE:GUEST_PATH[:ro]. Repeatable for up to 24
        /// volumes (vdb..vdy). The host file must be an existing ext4
        /// image (create one with `mkfs.ext4 /var/lib/forkd/vol/<tag>.img`).
        /// Use volumes for pip caches, model weights, agent scratch space —
        /// content survives across forks of the same tag.
        #[arg(long = "volume", value_name = "HOST:GUEST[:ro]")]
        volume: Vec<String>,
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
        /// Optional cgroup v2 memory limit per child, in MiB.
        /// Children exceeding this are OOM-killed by the kernel.
        /// Requires root or a delegated cgroup. See `crates/forkd-vmm/src/cgroup.rs`.
        #[arg(long)]
        memory_limit_mib: Option<u64>,
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
    /// Build a parent rootfs ext4 image from any Docker image.
    ///
    /// Replaces hand-running scripts/build-rootfs.sh. The resulting
    /// .ext4 ships with /forkd-init.sh + /forkd-agent.py preinstalled
    /// so the snapshot's PID 1 imports your deps and serves the agent.
    ///
    /// Example: forkd parent build python:3.12-slim --extra "python3-numpy"
    Parent {
        #[command(subcommand)]
        action: ParentAction,
    },
    /// Boot a one-shot sandbox without managing a snapshot tag.
    ///
    /// Equivalent to: build rootfs (if needed) → snapshot → fork 1 →
    /// exec command → shutdown. Suitable for "spawn me a quick sandbox"
    /// use cases (vs. the high-throughput `forkd fork` flow).
    ///
    /// Example: forkd run --image python:3.12-slim -- python -c 'print(1+1)'
    Run {
        /// Docker image to run.
        #[arg(long)]
        image: String,
        /// Extra apt packages to bake in.
        #[arg(long)]
        extra: Vec<String>,
        /// Image cache directory.
        #[arg(long, env = "FORKD_RUN_CACHE", default_value = "/var/cache/forkd")]
        cache: PathBuf,
        /// Kernel path.
        #[arg(long, env = "FORKD_KERNEL")]
        kernel: PathBuf,
        /// Host tap (created via scripts/host-tap.sh).
        #[arg(long, env = "FORKD_TAP", default_value = "forkd-tap0")]
        tap: String,
        /// Command to run (everything after `--`).
        #[arg(last = true)]
        cmd: Vec<String>,
    },
    /// Show where snapshots are stored.
    Where,
    /// Pack a local snapshot into a portable `.forkd-snapshot.tar.zst` file.
    ///
    /// Includes manifest.toml + per-file sha256, so `forkd unpack`/`pull`
    /// can verify integrity on the other end. Use this to ship a warmed
    /// snapshot to another host (or upload to the Snapshot Hub bucket).
    Pack {
        /// Tag of the local snapshot to pack.
        #[arg(long)]
        tag: String,
        /// Output file. Default: ./<sanitized-tag>.forkd-snapshot.tar.zst
        #[arg(long, short)]
        out: Option<PathBuf>,
        /// Human description recorded in the manifest. Optional.
        #[arg(long)]
        description: Option<String>,
        /// Upstream base image (e.g. `python:3.12-slim`). Informational.
        #[arg(long)]
        base_image: Option<String>,
    },
    /// Unpack a `.forkd-snapshot.tar.zst` into a local snapshot tag.
    ///
    /// Verifies every file's sha256 against the manifest. Refuses on
    /// pack-format mismatch or path traversal.
    Unpack {
        /// Pack file to read.
        path: PathBuf,
        /// Local tag to register under. Defaults to the manifest's `tag`.
        #[arg(long)]
        tag: Option<String>,
        /// Overwrite an existing local snapshot of the same tag.
        #[arg(long)]
        force: bool,
    },
    /// Download a pack from a URL and unpack into a local snapshot tag.
    ///
    /// MVP transport is plain HTTPS GET — point at an R2/S3 public URL
    /// (or a tag spec like `deeplethe/python-numpy`, which resolves via
    /// the default hub base URL).
    Pull {
        /// URL or `<owner>/<tag>` short form.
        target: String,
        /// Override the local tag (default: from manifest).
        #[arg(long)]
        tag: Option<String>,
        /// Overwrite an existing local snapshot of the same tag.
        #[arg(long)]
        force: bool,
        /// Hub base URL for short-form targets. Default: env FORKD_HUB_URL
        /// or https://forkd-hub.deeplethe.com.
        #[arg(long, env = "FORKD_HUB_URL")]
        hub: Option<String>,
    },
    /// List local snapshots with sizes.
    Images,
}

#[derive(Subcommand)]
enum ParentAction {
    /// Convert a Docker image into a writable ext4 rootfs.
    Build {
        /// Docker image (e.g. `python:3.12-slim`, `ubuntu:24.04`).
        image: String,
        /// Output ext4 file (default: ./<image-slug>.ext4).
        #[arg(long, short)]
        output: Option<PathBuf>,
        /// Image size in MiB (default 1536).
        #[arg(long, default_value_t = 1536)]
        size_mib: u32,
        /// Extra apt packages to install on top of the base image.
        #[arg(long)]
        extra: Vec<String>,
    },
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
            volume,
        } => snapshot_cmd(tag, kernel, rootfs, rw, tap, boot_wait_secs, volume),
        Cmd::Fork {
            tag,
            n,
            settle_secs,
            per_child_netns,
            memory_limit_mib,
        } => fork_cmd(tag, n, settle_secs, per_child_netns, memory_limit_mib),
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
        Cmd::Parent { action } => match action {
            ParentAction::Build {
                image,
                output,
                size_mib,
                extra,
            } => parent_build_cmd(image, output, size_mib, extra),
        },
        Cmd::Run {
            image,
            extra,
            cache,
            kernel,
            tap,
            cmd,
        } => run_cmd(image, extra, cache, kernel, tap, cmd),
        Cmd::Where => {
            println!("{}", data_dir().display());
            Ok(())
        }
        Cmd::Pack {
            tag,
            out,
            description,
            base_image,
        } => pack_cmd(tag, out, description, base_image),
        Cmd::Unpack { path, tag, force } => unpack_cmd(path, tag, force),
        Cmd::Pull {
            target,
            tag,
            force,
            hub,
        } => pull_cmd(target, tag, force, hub),
        Cmd::Images => images_cmd(),
    }
}

fn pack_cmd(
    tag: String,
    out: Option<PathBuf>,
    description: Option<String>,
    base_image: Option<String>,
) -> Result<()> {
    let snap_dir = snapshot_dir(&tag);
    if !snap_dir.exists() {
        bail!(
            "snapshot tag '{tag}' not found at {}\n\
             run 'forkd snapshot --tag {tag} ...' first",
            snap_dir.display()
        );
    }
    let out_path = out.unwrap_or_else(|| {
        let slug: String = tag
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '-' })
            .collect::<String>()
            .trim_matches('-')
            .to_string();
        PathBuf::from(format!("{slug}.forkd-snapshot.tar.zst"))
    });

    eprintln!("==> packing snapshot '{tag}' → {}", out_path.display());
    let t = Instant::now();
    let manifest = hub::pack(&tag, description, base_image, &snap_dir, &out_path)?;
    let written = std::fs::metadata(&out_path).map(|m| m.len()).unwrap_or(0);
    let total_uncompressed: u64 = manifest.files.iter().map(|f| f.size).sum();
    eprintln!(
        "✓ wrote {} ({} uncompressed; {:.1}× compression) in {:.1}s",
        hub::human_bytes(written),
        hub::human_bytes(total_uncompressed),
        if written > 0 {
            total_uncompressed as f64 / written as f64
        } else {
            0.0
        },
        t.elapsed().as_secs_f64(),
    );
    eprintln!(
        "  next: scp/upload, then `forkd unpack {}` on the target host",
        out_path.display()
    );
    Ok(())
}

fn unpack_cmd(path: PathBuf, tag: Option<String>, force: bool) -> Result<()> {
    if !path.exists() {
        bail!("pack file not found: {}", path.display());
    }
    eprintln!("==> unpacking {} ...", path.display());

    // Peek manifest first to know the destination tag.
    let tmp = std::env::temp_dir().join(format!("forkd-unpack-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).context("create temp dir")?;
    let manifest = hub::unpack(&path, &tmp)?;
    let final_tag = tag.unwrap_or(manifest.tag.clone());
    let dest = snapshot_dir(&final_tag);
    if dest.exists() {
        if !force {
            // Clean up the temp dir before bailing.
            let _ = std::fs::remove_dir_all(&tmp);
            bail!(
                "tag '{final_tag}' already exists at {}; pass --force to overwrite",
                dest.display()
            );
        }
        std::fs::remove_dir_all(&dest)
            .with_context(|| format!("remove existing {}", dest.display()))?;
    }
    std::fs::create_dir_all(dest.parent().unwrap()).ok();
    std::fs::rename(&tmp, &dest)
        .with_context(|| format!("move {} → {}", tmp.display(), dest.display()))?;
    eprintln!("✓ unpacked tag '{final_tag}' at {}", dest.display());
    eprintln!("  next: forkd fork --tag {final_tag} -n <N>");
    Ok(())
}

const DEFAULT_HUB_URL: &str = "https://forkd-hub.deeplethe.com";

fn pull_cmd(target: String, tag: Option<String>, force: bool, hub: Option<String>) -> Result<()> {
    let url = resolve_target_url(&target, hub.as_deref())?;
    let tmp_pack = std::env::temp_dir().join(format!("forkd-pull-{}.tar.zst", std::process::id()));
    let bytes = hub::download(&url, &tmp_pack)?;
    eprintln!("✓ downloaded {} ({})", hub::human_bytes(bytes), url);
    let r = unpack_cmd(tmp_pack.clone(), tag, force);
    let _ = std::fs::remove_file(&tmp_pack);
    r
}

fn resolve_target_url(target: &str, hub_base: Option<&str>) -> Result<String> {
    if target.starts_with("http://") || target.starts_with("https://") {
        return Ok(target.to_string());
    }
    // Short form: owner/tag
    if target.contains('/') && !target.contains(' ') {
        let base = hub_base.unwrap_or(DEFAULT_HUB_URL).trim_end_matches('/');
        let slug: String = target
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '/' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        return Ok(format!("{base}/{slug}.forkd-snapshot.tar.zst"));
    }
    bail!("invalid pull target '{target}'; expected an https URL or `<owner>/<tag>` short form")
}

fn images_cmd() -> Result<()> {
    let root = data_dir().join("snapshots");
    let infos = hub::list_local(&root)?;
    if infos.is_empty() {
        println!(
            "no local snapshots at {}\n  try: forkd snapshot --tag <name> ... or forkd pull <url>",
            root.display()
        );
        return Ok(());
    }
    println!("{:<32}  {:>12}  ROOTFS?", "TAG", "SIZE");
    for info in infos {
        println!(
            "{:<32}  {:>12}  {}",
            info.tag,
            hub::human_bytes(info.total_bytes),
            if info.has_rootfs { "yes" } else { "—" }
        );
    }
    Ok(())
}

fn parent_build_cmd(
    image: String,
    output: Option<PathBuf>,
    size_mib: u32,
    extra: Vec<String>,
) -> Result<()> {
    // Default output: ./<sanitized-image>.ext4 in current dir.
    let out = output.unwrap_or_else(|| {
        let slug: String = image
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '-' })
            .collect::<String>()
            .trim_matches('-')
            .to_string();
        PathBuf::from(format!("{slug}.ext4"))
    });

    // Locate scripts/build-rootfs.sh by walking up from the binary.
    let script = find_script("build-rootfs.sh")?;

    eprintln!(
        "==> forkd parent build:\n     image:   {}\n     output:  {}\n     size:    {} MiB\n     extras:  {}",
        image,
        out.display(),
        size_mib,
        if extra.is_empty() {
            "(none)".to_string()
        } else {
            extra.join(" ")
        }
    );

    // The build script does sudo internally for mount/chroot. Run as
    // current user; the user is expected to run `sudo forkd ...` once
    // for the whole pipeline (kvm + netns + bind mount all need root).
    let mut cmd = std::process::Command::new("bash");
    cmd.arg(&script)
        .arg(&image)
        .arg(&out)
        .arg(size_mib.to_string());
    for pkg in &extra {
        cmd.arg(pkg);
    }
    let status = cmd
        .status()
        .with_context(|| format!("failed to invoke bash {}", script.display()))?;
    if !status.success() {
        bail!(
            "build-rootfs.sh exited with status {status}.\n\
             Hint: this command needs root for docker + mkfs.ext4. Try `sudo -E forkd parent build ...`"
        );
    }
    eprintln!("✓ wrote {}", out.display());
    eprintln!(
        "  next: forkd snapshot --tag <name> --kernel <vmlinux> --rootfs {} --tap forkd-tap0",
        out.display()
    );
    Ok(())
}

/// `forkd run` — one-shot sandbox: build (if needed) → snapshot → fork → exec → kill.
fn run_cmd(
    image: String,
    extra: Vec<String>,
    cache: PathBuf,
    kernel: PathBuf,
    tap: String,
    cmd: Vec<String>,
) -> Result<()> {
    if cmd.is_empty() {
        bail!("no command provided. Usage: forkd run --image <img> -- <cmd> [args...]");
    }
    if !kernel.exists() {
        bail!(
            "kernel not found: {}\n\
             set --kernel or FORKD_KERNEL",
            kernel.display()
        );
    }

    // 1. Materialize the rootfs (cached).
    std::fs::create_dir_all(&cache).ok();
    let slug: String = image
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string();
    let rootfs = cache.join(format!("{slug}.ext4"));
    if !rootfs.exists() {
        eprintln!(
            "==> building rootfs for {image} (cached at {})",
            rootfs.display()
        );
        parent_build_cmd(image.clone(), Some(rootfs.clone()), 1536, extra)?;
    } else {
        eprintln!("==> using cached rootfs {}", rootfs.display());
    }

    // 2. Snapshot a one-off tag.
    let tag = format!("run-{slug}");
    eprintln!("==> snapshot --tag {tag}");
    snapshot_cmd(tag.clone(), kernel, rootfs, true, Some(tap), 10, Vec::new())?;

    // 3. Fork 1 child + exec the command via the guest agent.
    eprintln!("==> spawning sandbox and running command...");
    let snap_dir = snapshot_dir(&tag);
    let snapshot = load_snapshot_meta(&snap_dir)?;
    let work_dir = std::env::temp_dir().join(format!("forkd-run-{tag}"));
    let result = snapshot
        .restore_many_with(
            ForkOpts {
                n: 1,
                per_child_netns: false,
                memory_limit_mib: None,
            },
            &work_dir,
        )
        .context("restore_many failed")?;

    // Wait for the agent to come up, then exec the command.
    let target = "10.42.0.2:8888".to_string();
    let mut alive = false;
    for _ in 0..30 {
        if ping_at(&target).is_ok() {
            alive = true;
            break;
        }
        thread::sleep(Duration::from_millis(200));
    }
    if !alive {
        drop(result);
        bail!("sandbox agent never responded at {target}");
    }
    let resp = exec_at(&target, cmd, Duration::from_secs(60))?;
    if !resp.stdout.is_empty() {
        print!("{}", resp.stdout);
    }
    if !resp.stderr.is_empty() {
        eprint!("{}", resp.stderr);
    }
    let exit = resp.exit_code;

    // Shutdown.
    for c in &result.children {
        let _ = c.shutdown();
    }
    drop(result);

    std::process::exit(exit);
}

/// Locate a helper script in the repo's `scripts/` dir. Looks at common
/// install layouts: alongside the binary, under /usr/local/share/forkd,
/// or by walking up from the binary's parent.
fn find_script(name: &str) -> Result<PathBuf> {
    // Try $FORKD_SCRIPTS_DIR first.
    if let Ok(dir) = std::env::var("FORKD_SCRIPTS_DIR") {
        let p = PathBuf::from(dir).join(name);
        if p.exists() {
            return Ok(p);
        }
    }

    // Walk up from current_exe looking for `scripts/<name>`.
    if let Ok(mut exe) = std::env::current_exe() {
        for _ in 0..6 {
            exe.pop();
            let candidate = exe.join("scripts").join(name);
            if candidate.exists() {
                return Ok(candidate);
            }
        }
    }

    // Final fallback.
    let p = PathBuf::from("/usr/local/share/forkd/scripts").join(name);
    if p.exists() {
        return Ok(p);
    }

    bail!(
        "could not find scripts/{} — set FORKD_SCRIPTS_DIR to the forkd repo's `scripts/` dir",
        name
    )
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
    volume_specs: Vec<String>,
) -> Result<()> {
    if !kernel.exists() {
        bail!("kernel not found: {}", kernel.display());
    }
    if !rootfs.exists() {
        bail!("rootfs not found: {}", rootfs.display());
    }

    // Parse and validate volumes before booting so we fail early.
    let volumes: Vec<forkd_vmm::VolumeSpec> = volume_specs
        .iter()
        .map(|s| parse_volume(s))
        .collect::<Result<_>>()?;
    if volumes.len() > 24 {
        bail!("at most 24 volumes are supported (vdb..vdy)");
    }
    for v in &volumes {
        if !v.host_path.exists() {
            bail!(
                "volume host file not found: {}\n\
                 create it with: sudo dd if=/dev/zero of={} bs=1M count=512 && sudo mkfs.ext4 -F {}",
                v.host_path.display(),
                v.host_path.display(),
                v.host_path.display()
            );
        }
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

    for v in &volumes {
        eprintln!(
            "    volume: {} → {} ({})",
            v.host_path.display(),
            v.guest_path.display(),
            if v.read_only { "ro" } else { "rw" }
        );
        cfg = cfg.with_volume(v.clone());
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
    let snap = vm
        .snapshot_to(vmstate, memory, volumes)
        .context("snapshot create")?;
    eprintln!("    snapshot took {} ms", t.elapsed().as_millis());

    // Persist Snapshot metadata so subsequent `forkd fork` / `forkd run`
    // invocations recover the volume list (the vmstate file alone
    // doesn't carry our VolumeSpec annotations).
    let meta = serde_json::to_vec_pretty(&snap).context("serialize snapshot meta")?;
    std::fs::write(snap_dir.join("snapshot.json"), meta).context("write snapshot.json")?;

    vm.kill().context("kill parent")?;
    eprintln!("✓ tag '{tag}' ready. Try: forkd fork --tag {tag} --n 10");
    Ok(())
}

/// Load a `Snapshot` from `<snap_dir>/snapshot.json` if it exists,
/// otherwise fall back to constructing one from `vmstate` + `memory.bin`
/// with no volumes (backward compat for snapshots created before this
/// metadata file was introduced).
fn load_snapshot_meta(snap_dir: &std::path::Path) -> Result<Snapshot> {
    let meta_path = snap_dir.join("snapshot.json");
    if meta_path.exists() {
        let raw =
            std::fs::read(&meta_path).with_context(|| format!("read {}", meta_path.display()))?;
        let snap: Snapshot = serde_json::from_slice(&raw)
            .with_context(|| format!("parse {}", meta_path.display()))?;
        return Ok(snap);
    }
    Ok(Snapshot {
        vmstate: snap_dir.join("vmstate"),
        memory: snap_dir.join("memory.bin"),
        volumes: Vec::new(),
    })
}

/// Parse a `HOST:GUEST[:ro]` volume spec string.
fn parse_volume(s: &str) -> Result<forkd_vmm::VolumeSpec> {
    // Split into at most 3 parts so a colon inside a path (rare on Linux,
    // but possible) doesn't break parsing of the trailing `:ro` flag.
    let parts: Vec<&str> = s.splitn(3, ':').collect();
    if parts.len() < 2 || parts[0].is_empty() || parts[1].is_empty() {
        bail!("invalid --volume spec '{s}'; expected HOST:GUEST or HOST:GUEST:ro");
    }
    let read_only = match parts.get(2) {
        None => false,
        Some(&"ro") => true,
        Some(&"rw") => false,
        Some(other) => {
            bail!("invalid --volume spec '{s}'; trailing flag must be 'ro' or 'rw', got '{other}'")
        }
    };
    Ok(forkd_vmm::VolumeSpec {
        host_path: PathBuf::from(parts[0]),
        guest_path: PathBuf::from(parts[1]),
        read_only,
    })
}

fn fork_cmd(
    tag: String,
    n: usize,
    settle_secs: u64,
    per_child_netns: bool,
    memory_limit_mib: Option<u64>,
) -> Result<()> {
    let snap_dir = snapshot_dir(&tag);
    if !snap_dir.join("vmstate").exists() {
        bail!(
            "snapshot tag '{tag}' not found at {}\n\
             run 'forkd snapshot --tag {tag} ...' first",
            snap_dir.display()
        );
    }

    let snapshot = load_snapshot_meta(&snap_dir)?;
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
                memory_limit_mib,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_volume_basic() {
        let v = parse_volume("/var/lib/forkd/vol/pyagent.img:/opt/cache").unwrap();
        assert_eq!(v.host_path, PathBuf::from("/var/lib/forkd/vol/pyagent.img"));
        assert_eq!(v.guest_path, PathBuf::from("/opt/cache"));
        assert!(!v.read_only);
    }

    #[test]
    fn parse_volume_read_only() {
        let v = parse_volume("/var/lib/forkd/vol/models.img:/models:ro").unwrap();
        assert!(v.read_only);
    }

    #[test]
    fn parse_volume_explicit_rw() {
        let v = parse_volume("/host.img:/guest:rw").unwrap();
        assert!(!v.read_only);
    }

    #[test]
    fn parse_volume_rejects_missing_guest() {
        assert!(parse_volume("/host.img").is_err());
        assert!(parse_volume("/host.img:").is_err());
        assert!(parse_volume(":/guest").is_err());
    }

    #[test]
    fn parse_volume_rejects_bad_flag() {
        assert!(parse_volume("/host.img:/guest:wat").is_err());
    }
}
