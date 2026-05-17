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
    /// Boot a parent VM, warm it up, snapshot to disk — or, with
    /// `--from-sandbox`, snapshot a running child sandbox into a new
    /// tag via the controller daemon (sandbox branching).
    Snapshot {
        /// Name of the snapshot. Becomes ~/.local/share/forkd/snapshots/<tag>/.
        /// With `--from-sandbox`, leave unset to let the daemon generate
        /// `branch-<sandbox-id>-<unix-ts>`.
        #[arg(long)]
        tag: Option<String>,
        /// Branch a running sandbox instead of booting a fresh parent VM.
        /// Calls `POST /v1/sandboxes/<id>/branch` on the controller daemon
        /// (see --daemon-url / --daemon-token). The source sandbox is paused
        /// only for the duration of the snapshot (0.5–8 s typical).
        ///
        /// When set, `--kernel` / `--rootfs` / `--tap` / `--boot-wait-secs` /
        /// `--mem-size-mib` / `--volume` are ignored — the branch inherits
        /// those from the source's snapshot.
        #[arg(long, value_name = "SANDBOX_ID")]
        from_sandbox: Option<String>,
        /// Controller daemon base URL for `--from-sandbox` mode.
        #[arg(long, env = "FORKD_URL", default_value = "http://127.0.0.1:8889")]
        daemon_url: String,
        /// Bearer token for the controller daemon (matches `--token-file`).
        /// Read from the env var when unset.
        #[arg(long, env = "FORKD_TOKEN")]
        daemon_token: Option<String>,
        /// Path to vmlinux kernel.
        #[arg(long, env = "FORKD_KERNEL", required_unless_present = "from_sandbox")]
        kernel: Option<PathBuf>,
        /// Path to rootfs image. Pass `.ext4` for read-write, or `.squashfs` for read-only.
        #[arg(long, env = "FORKD_ROOTFS", required_unless_present = "from_sandbox")]
        rootfs: Option<PathBuf>,
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
        /// Parent VM memory size in MiB. Default 512 (set by BootConfig).
        /// Override for memory-hungry warmup workloads: browser recipes
        /// need ≥2048, larger SciPy / LLM warmups may need more.
        #[arg(long)]
        mem_size_mib: Option<u32>,
        /// Keep `/tmp/forkd-parent-<tag>/` after snapshot (default: remove).
        /// Useful for inspecting the parent VM console log post-snapshot.
        #[arg(long)]
        keep_workdir: bool,
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
        /// Keep `/tmp/forkd-fork-<tag>/` after shutdown (default: remove).
        /// Useful for post-mortem inspection of child console logs and
        /// Firecracker API sockets.
        #[arg(long)]
        keep_workdir: bool,
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
    /// Remove orphaned `/tmp/forkd-{fork,parent}-*` work directories.
    ///
    /// Each `forkd fork` / `forkd snapshot` creates a temp work dir holding
    /// Firecracker API sockets + console logs. They're removed at end of
    /// run by default, but can pile up if forkd crashes or is killed.
    /// This command sweeps the leftovers. Dry run by default — pass `--yes`
    /// to actually delete. Skips dirs that look like they have a live
    /// Firecracker (a `.sock` whose owning process is still running).
    Cleanup {
        /// Actually delete (default: list only).
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Push a local snapshot to a URL via HTTP PUT.
    ///
    /// MVP transport is plain HTTPS PUT — point at a presigned PUT URL
    /// from R2/S3/etc. (run `aws s3 presign --method PUT s3://bucket/key`
    /// or the R2 equivalent). The pack file is built on-the-fly in a
    /// temp dir and removed when the upload completes.
    Push {
        /// Local snapshot tag to pack and push.
        #[arg(long)]
        tag: String,
        /// Destination URL (presigned PUT). The bucket must accept this URL.
        url: String,
        /// Optional manifest description written into the pack.
        #[arg(long)]
        description: Option<String>,
        /// Optional upstream base image annotation.
        #[arg(long)]
        base_image: Option<String>,
    },
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

/// Validate a local snapshot tag.
///
/// Used everywhere we accept a `--tag` flag and also when reading
/// `manifest.toml` on unpack — without this, a tag like `/etc/x`
/// would land at `/etc/x` (Path::join silently discards the base
/// when the right side is absolute), and `../../../etc/x` would
/// climb out of the data dir on `forkd snapshot`. A malicious pack
/// on the Snapshot Hub could leverage the manifest tag for the same.
///
/// Allowed shape: `[A-Za-z0-9_][A-Za-z0-9._-]{0,63}` — 1-64 chars,
/// alphanumeric / dot / underscore / dash, must NOT lead with `.` or
/// `-` (avoids `..`, hidden-file looks, and CLI-confusing dash-leads).
fn validate_tag(tag: &str) -> Result<()> {
    if tag.is_empty() {
        bail!("invalid tag: empty");
    }
    if tag.len() > 64 {
        bail!(
            "invalid tag '{tag}': longer than 64 chars (got {})",
            tag.len()
        );
    }
    let first = tag.chars().next().unwrap();
    if !(first.is_ascii_alphanumeric() || first == '_') {
        bail!(
            "invalid tag '{tag}': must start with a letter, digit, or '_' \
             (got {first:?}). Tags cannot start with '.' or '-' or path separators."
        );
    }
    for c in tag.chars() {
        let ok = c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-';
        if !ok {
            bail!(
                "invalid tag '{tag}': illegal character {c:?}. \
                 Allowed: letters, digits, '.', '_', '-'."
            );
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();
    match cli.command {
        Cmd::Snapshot {
            tag,
            from_sandbox,
            daemon_url,
            daemon_token,
            kernel,
            rootfs,
            rw,
            tap,
            boot_wait_secs,
            mem_size_mib,
            keep_workdir,
            volume,
        } => snapshot_cmd(
            tag,
            from_sandbox,
            daemon_url,
            daemon_token,
            kernel,
            rootfs,
            rw,
            tap,
            boot_wait_secs,
            mem_size_mib,
            keep_workdir,
            volume,
        ),
        Cmd::Fork {
            tag,
            n,
            settle_secs,
            per_child_netns,
            memory_limit_mib,
            keep_workdir,
        } => fork_cmd(
            tag,
            n,
            settle_secs,
            per_child_netns,
            memory_limit_mib,
            keep_workdir,
        ),
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
        Cmd::Cleanup { yes } => cleanup_cmd(yes),
        Cmd::Push {
            tag,
            url,
            description,
            base_image,
        } => push_cmd(tag, url, description, base_image),
    }
}

fn pack_cmd(
    tag: String,
    out: Option<PathBuf>,
    description: Option<String>,
    base_image: Option<String>,
) -> Result<()> {
    validate_tag(&tag)?;
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
    // Validate caller-supplied tag up-front; the manifest's tag is
    // validated inside unpack_into() after we read it from the pack
    // (so a malicious pack with `tag = "../../etc/x"` is rejected too).
    if let Some(ref t) = tag {
        validate_tag(t)?;
    }
    eprintln!("==> unpacking {} ...", path.display());

    // Extract into a temp dir, then atomically rename into snapshot_dir.
    // On any error we make sure tmp is removed — previously a corrupted
    // tar.zst would leak /tmp/forkd-unpack-<pid>/ permanently.
    let tmp = std::env::temp_dir().join(format!("forkd-unpack-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).context("create temp dir")?;
    let result = unpack_into(&path, &tmp, tag, force);
    if result.is_err() {
        // Best-effort: if rename already moved tmp into dest, this is a
        // no-op; otherwise it removes the half-extracted scratch dir.
        let _ = std::fs::remove_dir_all(&tmp);
    }
    result
}

fn unpack_into(
    path: &std::path::Path,
    tmp: &std::path::Path,
    tag: Option<String>,
    force: bool,
) -> Result<()> {
    let manifest = hub::unpack(path, tmp)?;
    // Validate the manifest's declared tag *before* trusting it for
    // path computation. A pack uploaded by an attacker could declare
    // `tag = "../../etc/whatever"`; without this check, snapshot_dir()
    // would compute a path escape because Path::join silently keeps the
    // right side when it's absolute.
    let final_tag = match tag {
        Some(t) => t,
        None => {
            validate_tag(&manifest.tag).map_err(|e| {
                anyhow::anyhow!(
                    "pack manifest declares an invalid tag {:?}: {e}. \
                     Pass --tag <safe-name> to override.",
                    manifest.tag
                )
            })?;
            manifest.tag.clone()
        }
    };
    let dest = snapshot_dir(&final_tag);
    if dest.exists() {
        if !force {
            bail!(
                "tag '{final_tag}' already exists at {}; pass --force to overwrite",
                dest.display()
            );
        }
        std::fs::remove_dir_all(&dest)
            .with_context(|| format!("remove existing {}", dest.display()))?;
    }
    std::fs::create_dir_all(dest.parent().unwrap()).ok();
    std::fs::rename(tmp, &dest)
        .with_context(|| format!("move {} → {}", tmp.display(), dest.display()))?;
    eprintln!("✓ unpacked tag '{final_tag}' at {}", dest.display());
    eprintln!("  next: forkd fork --tag {final_tag} -n <N>");
    Ok(())
}

const DEFAULT_HUB_URL: &str = "https://forkd-hub.deeplethe.com";

fn pull_cmd(target: String, tag: Option<String>, force: bool, hub: Option<String>) -> Result<()> {
    if let Some(ref t) = tag {
        validate_tag(t)?;
    }
    let url = resolve_target_url(&target, hub.as_deref())?;
    let tmp_pack = std::env::temp_dir().join(format!("forkd-pull-{}.tar.zst", std::process::id()));
    // Clean tmp_pack whether download or unpack fails — both paths used
    // to leak /tmp/forkd-pull-<pid>.tar.zst on error.
    let result = (|| {
        let bytes = hub::download(&url, &tmp_pack)?;
        eprintln!("✓ downloaded {} ({})", hub::human_bytes(bytes), url);
        unpack_cmd(tmp_pack.clone(), tag, force)
    })();
    let _ = std::fs::remove_file(&tmp_pack);
    result
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

fn push_cmd(
    tag: String,
    url: String,
    description: Option<String>,
    base_image: Option<String>,
) -> Result<()> {
    validate_tag(&tag)?;
    let snap_dir = snapshot_dir(&tag);
    if !snap_dir.exists() {
        bail!(
            "snapshot tag '{tag}' not found at {}\n\
             run 'forkd snapshot --tag {tag} ...' first",
            snap_dir.display()
        );
    }
    let tmp_pack = std::env::temp_dir().join(format!(
        "forkd-push-{}-{}.tar.zst",
        std::process::id(),
        tag.chars()
            .filter(|c| c.is_alphanumeric())
            .collect::<String>()
    ));

    eprintln!("==> packing snapshot '{tag}' → {}", tmp_pack.display());
    let t = Instant::now();
    let manifest = hub::pack(&tag, description, base_image, &snap_dir, &tmp_pack)?;
    let pack_size = std::fs::metadata(&tmp_pack).map(|m| m.len()).unwrap_or(0);
    let total_uncompressed: u64 = manifest.files.iter().map(|f| f.size).sum();
    eprintln!(
        "    packed {} ({:.1}× compression) in {:.1}s",
        hub::human_bytes(pack_size),
        if pack_size > 0 {
            total_uncompressed as f64 / pack_size as f64
        } else {
            0.0
        },
        t.elapsed().as_secs_f64(),
    );

    let upload_t = Instant::now();
    let r = hub::upload(&tmp_pack, &url);
    // Clean up the temp pack whether the upload worked or not.
    let _ = std::fs::remove_file(&tmp_pack);
    let uploaded = r?;
    eprintln!(
        "✓ pushed {} in {:.1}s ({:.1} MiB/s)",
        hub::human_bytes(uploaded),
        upload_t.elapsed().as_secs_f64(),
        (uploaded as f64) / 1024.0 / 1024.0 / upload_t.elapsed().as_secs_f64().max(0.001),
    );
    Ok(())
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
    snapshot_cmd(
        Some(tag.clone()),
        None,                                // from_sandbox
        "http://127.0.0.1:8889".to_string(), // daemon_url (unused in local-boot path)
        None,                                // daemon_token
        Some(kernel),
        Some(rootfs),
        true,
        Some(tap),
        10,
        None,
        false,
        Vec::new(),
    )?;

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
        if let Some(stk) = v.get("stack").and_then(|s| s.as_str()) {
            eprintln!("{stk}");
        }
        std::process::exit(1);
    }
    // Node-bridge recipes return `result_json` (JSON-encoded value);
    // Python recipes return `result` (a repr() string). Print whichever
    // is present so the same CLI works against both kinds of snapshot.
    if let Some(r) = v.get("result_json").and_then(|r| r.as_str()) {
        println!("{r}");
    } else if let Some(r) = v.get("result").and_then(|r| r.as_str()) {
        println!("{r}");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)] // mirrors the CLI flag surface 1-to-1
fn snapshot_cmd(
    tag: Option<String>,
    from_sandbox: Option<String>,
    daemon_url: String,
    daemon_token: Option<String>,
    kernel: Option<PathBuf>,
    rootfs: Option<PathBuf>,
    rw_flag: bool,
    tap: Option<String>,
    boot_wait_secs: u64,
    mem_size_mib: Option<u32>,
    keep_workdir: bool,
    volume_specs: Vec<String>,
) -> Result<()> {
    // Branch path: snapshot a running sandbox via the controller daemon.
    // Skips the local boot + warmup loop entirely; daemon owns the source VM.
    if let Some(sandbox_id) = from_sandbox {
        return branch_snapshot_via_daemon(&daemon_url, daemon_token, &sandbox_id, tag);
    }

    let tag =
        tag.ok_or_else(|| anyhow::anyhow!("--tag is required unless --from-sandbox is set"))?;
    let kernel = kernel
        .ok_or_else(|| anyhow::anyhow!("--kernel is required unless --from-sandbox is set"))?;
    let rootfs = rootfs
        .ok_or_else(|| anyhow::anyhow!("--rootfs is required unless --from-sandbox is set"))?;

    validate_tag(&tag)?;
    if !kernel.exists() {
        bail!("kernel not found: {}", kernel.display());
    }
    if !rootfs.exists() {
        bail!("rootfs not found: {}", rootfs.display());
    }
    let work_dir_check = std::env::temp_dir().join(format!("forkd-parent-{tag}"));
    preflight_workdir(&work_dir_check, "snapshot", &tag)?;

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
    if let Some(mib) = mem_size_mib {
        eprintln!("    memory: {mib} MiB (override; default is 512)");
        cfg.mem_size_mib = mib;
    }

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

    // Parent VM is dead and the snapshot lives under data_dir; work_dir
    // (Firecracker API socket + console log) is now scratch.
    if keep_workdir {
        eprintln!(
            "    work_dir kept (per --keep-workdir): {}",
            work_dir.display()
        );
    } else {
        cleanup_workdir(&work_dir);
    }

    eprintln!("✓ tag '{tag}' ready. Try: forkd fork --tag {tag} --n 10");
    Ok(())
}

/// Best-effort recursive remove of a forkd work_dir. Logs but does not
/// fail the command if the directory can't be removed (e.g. a stale
/// process still has a socket open). Refuses to touch anything outside
/// `/tmp/forkd-` to keep --keep-workdir / cleanup behaviour safe.
fn cleanup_workdir(work_dir: &std::path::Path) {
    if !work_dir.exists() {
        return;
    }
    let s = work_dir.to_string_lossy();
    if !s.starts_with("/tmp/forkd-") {
        eprintln!(
            "    refusing to clean work_dir outside /tmp/forkd-*: {}",
            work_dir.display()
        );
        return;
    }
    match std::fs::remove_dir_all(work_dir) {
        Ok(()) => eprintln!("    cleaned work_dir {}", work_dir.display()),
        Err(e) => eprintln!(
            "    note: could not clean work_dir {}: {e}\n          \
             run `forkd cleanup --yes` later if it sticks",
            work_dir.display()
        ),
    }
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

/// `forkd snapshot --from-sandbox <id>` path: POST the running sandbox's
/// branch endpoint and print the resulting SnapshotInfo. Maps any non-2xx
/// response into a user-readable error so the operator sees the daemon's
/// JSON error body.
fn branch_snapshot_via_daemon(
    daemon_url: &str,
    token: Option<String>,
    sandbox_id: &str,
    tag: Option<String>,
) -> Result<()> {
    let url = format!(
        "{}/v1/sandboxes/{}/branch",
        daemon_url.trim_end_matches('/'),
        sandbox_id
    );
    let body = match tag.as_deref() {
        Some(t) => {
            validate_tag(t)?;
            serde_json::json!({ "tag": t }).to_string()
        }
        None => "{}".to_string(),
    };
    eprintln!("==> POST {url}");

    let mut req = ureq::post(&url).set("Content-Type", "application/json");
    if let Some(t) = token.as_deref() {
        req = req.set("Authorization", &format!("Bearer {t}"));
    }

    let resp = match req.send_string(&body) {
        Ok(r) => r,
        Err(ureq::Error::Status(code, r)) => {
            let body = r.into_string().unwrap_or_default();
            bail!("daemon returned HTTP {code}: {body}");
        }
        Err(e) => return Err(anyhow::anyhow!("HTTP POST failed: {e}")),
    };

    let body_str = resp.into_string().context("read daemon response body")?;
    let info: serde_json::Value =
        serde_json::from_str(&body_str).context("parse daemon JSON response")?;
    let new_tag = info["tag"].as_str().unwrap_or("?");
    let dir = info["dir"].as_str().unwrap_or("?");
    let branched_from = info["branched_from"].as_str().unwrap_or("?");
    eprintln!("✓ branch ready");
    println!("tag:           {new_tag}");
    println!("dir:           {dir}");
    println!("branched_from: {branched_from}");
    Ok(())
}

fn fork_cmd(
    tag: String,
    n: usize,
    settle_secs: u64,
    per_child_netns: bool,
    memory_limit_mib: Option<u64>,
    keep_workdir: bool,
) -> Result<()> {
    validate_tag(&tag)?;
    let snap_dir = snapshot_dir(&tag);
    if !snap_dir.join("vmstate").exists() {
        bail!(
            "snapshot tag '{tag}' not found at {}\n\
             run 'forkd snapshot --tag {tag} ...' first",
            snap_dir.display()
        );
    }
    let work_dir_check = std::env::temp_dir().join(format!("forkd-fork-{tag}"));
    preflight_workdir(&work_dir_check, "fork", &tag)?;

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

    // Children are dead; sockets + console logs in work_dir are orphans.
    if keep_workdir {
        eprintln!(
            "==> work_dir kept (per --keep-workdir): {}",
            work_dir.display()
        );
    } else {
        cleanup_workdir(&work_dir);
    }

    Ok(())
}

/// `forkd cleanup` — sweep orphaned `/tmp/forkd-{fork,parent}-*` work
/// directories left behind by crashed or killed forkd runs.
fn cleanup_cmd(yes: bool) -> Result<()> {
    let tmp = std::env::temp_dir();
    // Prefixes for transient state forkd creates. Pull/unpack add their
    // own scratch dirs (forkd-unpack-<pid>, forkd-pull-*) — sweep all
    // of them, not just the fork/parent work_dirs.
    const PREFIXES: &[&str] = &[
        "forkd-fork-",
        "forkd-parent-",
        "forkd-unpack-",
        "forkd-pull-",
    ];
    let matches_prefix = |name: &str| PREFIXES.iter().any(|p| name.starts_with(p));

    let mut candidates: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(&tmp).with_context(|| format!("read {}", tmp.display()))? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !matches_prefix(&name) {
            continue;
        }
        // Sweep both dirs (fork/parent/unpack) and files (pull-*.tar.zst).
        candidates.push(entry.path());
    }
    candidates.sort();

    if candidates.is_empty() {
        println!("no orphan work_dirs under {}", tmp.display());
        return Ok(());
    }

    println!("{} candidate work_dir(s):", candidates.len());
    let mut targets: Vec<PathBuf> = Vec::new();
    for dir in &candidates {
        let live = workdir_has_live_process(dir);
        let size = dir_size_bytes(dir).unwrap_or(0);
        if live {
            println!(
                "  SKIP {:<48}  {:>10}  (live socket — a forkd run looks active)",
                dir.display(),
                hub::human_bytes(size)
            );
        } else {
            println!(
                "  DEL  {:<48}  {:>10}",
                dir.display(),
                hub::human_bytes(size)
            );
            targets.push(dir.clone());
        }
    }

    if targets.is_empty() {
        println!("nothing safe to remove.");
        return Ok(());
    }
    if !yes {
        println!();
        println!(
            "dry run — pass `--yes` to delete the {} dir(s) marked DEL above.",
            targets.len()
        );
        return Ok(());
    }
    let tmp_prefix = format!("{}/", tmp.display());
    for dir in &targets {
        // Belt-and-suspenders: refuse anything outside /tmp/, and require
        // the name to start with one of our known prefixes.
        let s = dir.to_string_lossy();
        let Some(file_name) = dir.file_name().and_then(|n| n.to_str()) else {
            eprintln!("  REFUSE {} — unreadable file name", dir.display());
            continue;
        };
        if !s.starts_with(&tmp_prefix) || !matches_prefix(file_name) {
            eprintln!("  REFUSE {} — unexpected path", dir.display());
            continue;
        }
        let res = if dir.is_dir() {
            std::fs::remove_dir_all(dir)
        } else {
            std::fs::remove_file(dir)
        };
        match res {
            Ok(()) => println!("  removed {}", dir.display()),
            Err(e) => eprintln!("  ! failed to remove {}: {e}", dir.display()),
        }
    }
    Ok(())
}

/// Pre-flight check before `forkd snapshot` / `forkd fork` boots a new
/// VM under work_dir. Refuses if another forkd run on the same tag is
/// already in flight (live API socket holder); otherwise removes any
/// leftover stale work_dir so the upcoming run starts clean.
///
/// Without this, two concurrent `forkd fork --tag X` runs end up
/// stepping on each other's sockets and surfacing a cryptic
/// Firecracker-side `Resource busy` error for the tap device. We
/// now fail fast with a forkd-level explanation instead.
fn preflight_workdir(work_dir: &std::path::Path, op: &str, tag: &str) -> Result<()> {
    if !work_dir.exists() {
        return Ok(());
    }
    if workdir_has_live_process(work_dir) {
        bail!(
            "another `forkd {op}` looks active on tag '{tag}' — its work_dir \
             at {} still has a live Firecracker process holding sockets. \
             Wait for the other run to finish (or kill it) before re-running. \
             If you're sure nothing's alive, run `forkd cleanup --yes`.",
            work_dir.display()
        );
    }
    // Stale work_dir from a previous run that exited without cleaning up
    // (--keep-workdir, SIGKILL, crash). Safe to remove since no live
    // process is holding it.
    let s = work_dir.to_string_lossy();
    if !s.starts_with("/tmp/forkd-fork-") && !s.starts_with("/tmp/forkd-parent-") {
        bail!("internal error: preflight refusing unexpected path: {s}");
    }
    eprintln!(
        "    note: cleaning stale work_dir {} from a previous run",
        work_dir.display()
    );
    std::fs::remove_dir_all(work_dir).with_context(|| {
        format!(
            "remove stale work_dir {} (run `forkd cleanup --yes` if this keeps failing)",
            work_dir.display()
        )
    })?;
    Ok(())
}

/// "Is any process currently using this work_dir?"
///
/// Walks `/proc/*/fd/*` symlinks and returns `true` if any of them
/// resolves to a path inside `work_dir`. Firecracker has stdout
/// redirected to `<work_dir>/child-N.console`, so a live VM always
/// holds an open fd whose readlink target starts with our work_dir
/// prefix.
///
/// Why not /proc/*/cmdline (the previous impl)?
///   - false positives: any shell command or text editor that
///     happens to mention the work_dir path in its argv — including
///     the shell that *runs* `forkd cleanup` — gets flagged as live.
///   - The fd scan answers the actual question we care about:
///     "does anyone hold an open handle inside this directory?"
///
/// Why not lsof?
///   - lsof against a Firecracker UNIX-domain API socket returns
///     warnings on stderr and zero rows on stdout, so trusting
///     empty stdout meant treating live VMs as dead. (That bug
///     shipped in PR #35 and was fixed in PR #36.)
///
/// Errs on the side of "live" (skip the dir) whenever we can't
/// decide: /proc unreadable, fd race during scan, EACCES on a fd
/// we don't own.
fn workdir_has_live_process(dir: &std::path::Path) -> bool {
    // Canonicalise so symlink targets compare cleanly; fall back to
    // raw path if canonicalize fails (e.g. dir doesn't exist yet).
    let dir_buf = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let Ok(proc_root) = std::fs::read_dir("/proc") else {
        return true;
    };
    for pid_entry in proc_root.flatten() {
        let name = pid_entry.file_name();
        let name = name.to_string_lossy();
        if !name.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let fd_dir = pid_entry.path().join("fd");
        let Ok(fds) = std::fs::read_dir(&fd_dir) else {
            // No permission to inspect this PID — be conservative.
            // We're under sudo for the calls that matter here.
            continue;
        };
        for fd in fds.flatten() {
            let Ok(target) = std::fs::read_link(fd.path()) else {
                continue;
            };
            if target.starts_with(&dir_buf) {
                return true;
            }
        }
    }
    false
}

/// Sum the file sizes under `dir` (single-level, since work_dirs are flat).
fn dir_size_bytes(dir: &std::path::Path) -> std::io::Result<u64> {
    let mut total: u64 = 0;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if let Ok(m) = entry.metadata() {
            if m.is_file() {
                total += m.len();
            }
        }
    }
    Ok(total)
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
