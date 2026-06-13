//! `forkd` — CLI entrypoint.
//!
//! Subcommands:
//!
//! ```text
//!   forkd snapshot --tag <name> --kernel <path> --rootfs <path>
//!   forkd fork --tag <name> --n <N>
//!   forkd pack --tag <name> [--out <file>]    (Snapshot Hub)
//!   forkd unpack <file> [--tag <name>]        (Snapshot Hub)
//!   forkd pull <url> [--tag <name>]           (Snapshot Hub)
//!   forkd images                              (Snapshot Hub)
//! ```
//!
//! Snapshots live under `$XDG_DATA_HOME/forkd/snapshots/<tag>/`.

mod bench;
mod doctor;
mod hub;
mod sandbox;
mod wp_bench;

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
    /// Zero-to-first-fork in one command. Preflights the host, heals
    /// what's missing with your consent (guest kernel download, tap
    /// device, per-child netns), bakes a Python snapshot from Docker
    /// (or pulls one from the hub when Docker is absent), then forks
    /// N children and prints per-child timings.
    ///
    /// Run as: sudo -E forkd quickstart
    Quickstart {
        /// How many children to fork for the demo.
        #[arg(long, short, default_value_t = 10)]
        n: usize,
        /// Docker image the parent snapshot is baked from. Ignored when
        /// Docker is unavailable (hub pull route).
        #[arg(long, default_value = "python:3.12-slim")]
        image: String,
        /// Answer yes to all setup prompts (kernel download, tap
        /// creation, netns provisioning) — for non-interactive use.
        #[arg(long, short)]
        yes: bool,
    },
    /// Boot a parent VM, warm it up, snapshot to disk — or, with
    /// `--from-sandbox`, snapshot a running child sandbox into a new
    /// tag via the controller daemon (sandbox branching).
    Snapshot {
        /// Name of the snapshot. Becomes `~/.local/share/forkd/snapshots/<tag>/`.
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
        /// Use v0.3 Diff snapshot mode (only with `--from-sandbox`).
        /// Source pause collapses to ~200 ms vs seconds for Full. Multi-
        /// BRANCH supported in v0.3.1+ via the previous-output chain.
        /// Ignored when `--from-sandbox` is not set. Mutually exclusive
        /// with `--live`.
        #[arg(long, conflicts_with = "live")]
        diff: bool,
        /// Use v0.4 Live (UFFD_WP-based) BRANCH mode. Source pause drops to sub-50 ms;
        /// memory streamed asynchronously from the running parent. Requires the source
        /// sandbox to have been created with `--live-fork` (memfd-backed RAM, kernel
        /// 5.7+, vendored Firecracker fork — see docs/VENDORED-FIRECRACKER.md).
        /// Mutually exclusive with `--diff`. Ignored when `--from-sandbox` is not set.
        #[arg(long, conflicts_with = "diff")]
        live: bool,
        /// With `--live`, return as soon as the source resumes
        /// (~10 ms) instead of waiting for the background memory
        /// copy to finish. Snapshot reaches `status: ready` later;
        /// poll `forkd ls --snapshots` (or `GET /v1/snapshots`).
        /// Requires `--live`.
        #[arg(long, requires = "live")]
        no_wait: bool,
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
        /// Format: `HOST_FILE:GUEST_PATH[:ro]`. Repeatable for up to 24
        /// volumes (vdb..vdy). The host file must be an existing ext4
        /// image (create one with `mkfs.ext4 /var/lib/forkd/vol/<tag>.img`).
        /// Use volumes for pip caches, model weights, agent scratch space —
        /// content survives across forks of the same tag.
        #[arg(long = "volume", value_name = "HOST:GUEST[:ro]")]
        volume: Vec<String>,
    },
    /// Derive a new diff snapshot from a base tag by running an
    /// installer in a transient sandbox (v0.5 / ROADMAP M2.1).
    ///
    /// Spawns a one-shot sandbox from `--from`, waits for the guest
    /// agent, runs `--exec` via the agent, then BRANCHes a Diff
    /// snapshot tagged `--tag` whose `parent_tag` records the chain
    /// edge back to `--from`. The sandbox is killed when done.
    ///
    /// At restore time the controller walks the chain (Phase 2a) and
    /// assembles memory via `cp(base) + apply_diff(each link)`. Each
    /// link's `parent_content_hash` pins the parent's bytes so
    /// re-snapshotting the base under the same tag fails loudly
    /// rather than silently restoring the wrong bytes.
    ///
    /// Example:
    ///   forkd snapshot-diff --from python-numpy --tag python-numpy+pandas \
    ///       --exec "pip install pandas==2.0.0"
    ///
    /// Constraints (v0.5): the base must be registered with the
    /// daemon (`POST /v1/snapshots` or `forkd snapshot --tag`). The
    /// exec command runs inside a sandbox that inherits the base's
    /// network namespace — outbound HTTPS to PyPI / apt mirrors
    /// works via the daemon's tap device. Restore-time chain
    /// performance is best on a reflink-capable host filesystem;
    /// `forkd doctor` will warn on non-reflink hosts in a follow-up.
    SnapshotDiff {
        /// Base snapshot tag to derive from. Must already be
        /// registered (run `forkd ls --snapshots` or check
        /// `~/.local/share/forkd/snapshots/`).
        #[arg(long)]
        from: String,
        /// Tag for the new diff snapshot. Will be registered with
        /// the daemon and persisted under
        /// `~/.local/share/forkd/snapshots/<tag>/`.
        #[arg(long)]
        tag: String,
        /// Command to run in the spawned sandbox to produce the
        /// delta. Quoted shell command (e.g.
        /// `--exec "pip install pandas==2.0.0"`); split on
        /// whitespace and passed to the guest agent's exec endpoint.
        #[arg(long)]
        exec: String,
        /// Maximum wall-clock seconds for the exec step. Defaults
        /// to 600 (10 min) — long installs like `pip install torch`
        /// can hit this; bump as needed.
        #[arg(long, default_value_t = 600)]
        exec_timeout_secs: u64,
        /// Controller daemon base URL.
        #[arg(long, env = "FORKD_URL", default_value = "http://127.0.0.1:8889")]
        daemon_url: String,
        /// Bearer token for the controller daemon.
        #[arg(long, env = "FORKD_TOKEN")]
        daemon_token: Option<String>,
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
        /// Boot each child with a memfd-backed RAM region (v0.4). Required
        /// upfront if you later want to take a v0.4 live BRANCH off this
        /// child — `mode: "live"` arms UFFD_WP on the memfd, which only
        /// works on shmem-backed VMAs (kernel 5.7+ + vendored Firecracker
        /// fork; see `docs/VENDORED-FIRECRACKER.md`). `forkd doctor`
        /// probes both prereqs. No effect at spawn time beyond the
        /// backend swap; cost shows up on the first live BRANCH.
        #[arg(long)]
        live_fork: bool,
        /// Back the memfd with 2 MiB hugepages (`MFD_HUGETLB | MFD_HUGE_2MB`).
        /// Only meaningful with `--live-fork`. Reduces TLB pressure during
        /// spawn-many and live BRANCH bulk-copy. Requires hugepages to be
        /// reserved on the host (`echo N > /proc/sys/vm/nr_hugepages`);
        /// `forkd doctor` checks availability. Falls back to normal pages
        /// with a warning if the pool is exhausted.
        #[arg(long, requires = "live_fork")]
        hugepages: bool,
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
    /// Build a forkd snapshot from a Docker image in one command.
    ///
    /// Wraps the `forkd parent build` (Docker → ext4) → `forkd snapshot`
    /// (boot + warmup + pause + register tag) pipeline. After this
    /// completes you can `forkd fork --tag <tag>` and start forking
    /// children from the warmed parent.
    ///
    /// Example: forkd from-image python:3.12-slim --tag py-numpy --extra python3-numpy
    FromImage {
        /// Docker image reference (e.g., `python:3.12-slim`,
        /// `ghcr.io/user/repo:tag`, `registry.example.com/foo:bar`).
        image: String,
        /// Forkd snapshot tag to register the result under.
        #[arg(long)]
        tag: String,
        /// Extra apt packages to install into the rootfs.
        #[arg(long)]
        extra: Vec<String>,
        /// Rootfs size in MiB.
        #[arg(long, default_value_t = 1536)]
        size_mib: u32,
        /// Cache directory for built rootfs artifacts (so re-running
        /// with the same image skips the Docker → ext4 step).
        #[arg(long, env = "FORKD_RUN_CACHE", default_value = "/var/cache/forkd")]
        cache: PathBuf,
        /// Kernel image. If unset, searches `./vmlinux-6.1.141`,
        /// `./vmlinux`, `/var/lib/forkd/kernels/vmlinux`, and
        /// `/usr/local/share/forkd/vmlinux`.
        #[arg(long, env = "FORKD_KERNEL")]
        kernel: Option<PathBuf>,
        /// Host tap device for the boot warmup.
        #[arg(long, env = "FORKD_TAP", default_value = "forkd-tap0")]
        tap: String,
        /// Seconds to wait for the guest to settle after boot before
        /// snapshotting.
        #[arg(long, default_value_t = 10)]
        boot_wait_secs: u64,
        /// Parent VM memory size in MiB. Default 512 (set by BootConfig).
        #[arg(long)]
        mem_size_mib: Option<u32>,
    },
    /// Remove one or more snapshot tags. Tries the daemon's DELETE
    /// /v1/snapshots/:tag first (clean: removes registry entry + on-disk
    /// files atomically); falls back to direct disk removal if the
    /// daemon isn't running.
    ///
    /// Examples:
    ///   forkd rmi pyagent
    ///   forkd rmi pyagent langgraph python-numpy
    Rmi {
        /// Snapshot tags to remove.
        tags: Vec<String>,
        /// v0.5 Phase 4: delete this snapshot AND every snapshot
        /// chained off it (and their descendants). Mutually exclusive
        /// with `--force`.
        #[arg(long)]
        cascade: bool,
        /// v0.5 Phase 4: delete this snapshot even if it would orphan
        /// child snapshots (chain broken — children become un-restorable).
        /// Mutually exclusive with `--cascade`.
        #[arg(long)]
        force: bool,
        /// Controller daemon base URL.
        #[arg(long, env = "FORKD_URL", default_value = "http://127.0.0.1:8889")]
        daemon_url: String,
        /// Bearer token (matches the daemon's --token-file).
        #[arg(long, env = "FORKD_TOKEN")]
        daemon_token: Option<String>,
    },
    /// Show chain info for a snapshot (`GET /v1/snapshots/:tag/info`):
    /// parent chain, dependents, on-disk sizes. v0.5 Phase 4.
    ///
    /// Use before `rmi`-ing a chained snapshot to see which children
    /// you'd orphan.
    SnapshotInfo {
        /// Snapshot tag to query.
        tag: String,
        /// Print the daemon's JSON body verbatim instead of the
        /// human-formatted table.
        #[arg(long)]
        json: bool,
        /// Controller daemon base URL.
        #[arg(long, env = "FORKD_URL", default_value = "http://127.0.0.1:8889")]
        daemon_url: String,
        /// Bearer token (matches the daemon's --token-file).
        #[arg(long, env = "FORKD_TOKEN")]
        daemon_token: Option<String>,
    },
    /// Flatten a chained snapshot into a new base snapshot
    /// (`POST /v1/snapshots/:tag/compact`). v0.5 Phase 4.
    ///
    /// Useful when chain depth has grown enough that the per-link
    /// SHA-256-of-base spawn tax is biting. The new tag has
    /// `parent_tag = None` and restores via the original Phase 1
    /// non-chain path.
    SnapshotCompact {
        /// Source snapshot tag (the chain head you want flattened).
        #[arg(long)]
        from: String,
        /// Tag for the new flat snapshot. Must not already exist.
        #[arg(long)]
        to: String,
        /// Controller daemon base URL.
        #[arg(long, env = "FORKD_URL", default_value = "http://127.0.0.1:8889")]
        daemon_url: String,
        /// Bearer token (matches the daemon's --token-file).
        #[arg(long, env = "FORKD_TOKEN")]
        daemon_token: Option<String>,
    },
    /// List live sandboxes (GET /v1/sandboxes). Table output.
    Ls {
        /// Controller daemon base URL.
        #[arg(long, env = "FORKD_URL", default_value = "http://127.0.0.1:8889")]
        daemon_url: String,
        /// Bearer token (matches the daemon's --token-file).
        #[arg(long, env = "FORKD_TOKEN")]
        daemon_token: Option<String>,
    },
    /// Kill one or more sandboxes (DELETE /v1/sandboxes/:id).
    ///
    /// Examples:
    ///   forkd kill sb-abc-0000
    ///   forkd kill sb-abc-0000 sb-abc-0001
    ///   forkd kill --all
    ///   forkd kill --tag pyagent
    Kill {
        /// Sandbox IDs to kill. Repeatable; ignored if --all or --tag is set.
        ids: Vec<String>,
        /// Kill every live sandbox the daemon knows about.
        #[arg(long, conflicts_with = "tag")]
        all: bool,
        /// Kill every sandbox forked from this snapshot tag.
        #[arg(long, conflicts_with = "all")]
        tag: Option<String>,
        /// Controller daemon base URL.
        #[arg(long, env = "FORKD_URL", default_value = "http://127.0.0.1:8889")]
        daemon_url: String,
        /// Bearer token (matches the daemon's --token-file).
        #[arg(long, env = "FORKD_TOKEN")]
        daemon_token: Option<String>,
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
        /// Output file. Default: `./<sanitized-tag>.forkd-snapshot.tar.zst`
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
        /// Hub base URL for short-form targets. Default: env `FORKD_HUB_URL`
        /// or <https://forkd-hub.deeplethe.com>.
        #[arg(long, env = "FORKD_HUB_URL")]
        hub: Option<String>,
    },
    /// List local snapshots with sizes.
    Images,
    /// Diagnose host setup. Checks KVM, tap device, ip_forward, netns,
    /// firecracker binary, kernel image, snapshot dir, and the
    /// controller daemon. Surfaces fix hints for each failed check.
    ///
    /// Safe to run unprivileged — skips checks that need root with a
    /// note. Run this first after a fresh `scripts/setup-host.sh`.
    Doctor {
        /// Controller daemon base URL for the daemon-reachable check.
        #[arg(long, env = "FORKD_URL", default_value = "http://127.0.0.1:8889")]
        daemon_url: String,
        /// Bearer token for the controller daemon (matches `--token-file`).
        #[arg(long, env = "FORKD_TOKEN")]
        daemon_token: Option<String>,
    },
    /// Quick latency probe against a live daemon. Runs a representative
    /// spawn → exec → branch (diff=true) → fanout → cleanup cycle and
    /// prints per-step timing. Screenshot-friendly output.
    ///
    /// Useful for: "is forkd actually fast on this box?", regression
    /// checks after a config change, and showing the v0.3 numbers
    /// reproduce on your hardware.
    Bench {
        /// Snapshot tag to spawn from. Defaults to the first snapshot
        /// the daemon knows about.
        #[arg(long)]
        tag: Option<String>,
        /// Fanout: how many grandchildren to spawn from the BRANCH.
        #[arg(long, default_value_t = 5)]
        n: usize,
        /// Per-child netns for the fanout. Defaults to true (the
        /// fanout will fail without per-child netns when n > 1).
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        per_child_netns: bool,
        /// Controller daemon base URL.
        #[arg(long, env = "FORKD_URL", default_value = "http://127.0.0.1:8889")]
        daemon_url: String,
        /// Bearer token for the controller daemon (matches `--token-file`).
        #[arg(long, env = "FORKD_TOKEN")]
        daemon_token: Option<String>,
    },
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

    /// Stateful workspaces (#116) — long-lived sandboxes that survive
    /// suspend / resume across daemon restarts. Drive via the
    /// controller daemon (`FORKD_URL` / `FORKD_TOKEN`).
    #[command(subcommand)]
    Workspace(WorkspaceAction),

    /// v0.4 prototype: exercise the snapshot-side UFFD_WP machinery
    /// on a synthetic memfd, outside the Firecracker integration.
    ///
    /// Creates a memfd of the requested size, populates with a known
    /// pattern, arms `UFFDIO_WRITEPROTECT`, runs the bulk-copy + handler
    /// pair, finalizes, and prints timing. Useful for benchmarking
    /// `arm_duration` and `bulk_copy_clean` throughput on a given
    /// kernel / filesystem combination before committing to the full
    /// BRANCH integration (tracked in DESIGN-v0.4.md).
    ///
    /// Linux x86_64, kernel >= 5.7. Either run as root or set
    /// `sysctl vm.unprivileged_userfaultfd=1`.
    WpBench {
        /// Region size in MiB.
        #[arg(long, default_value_t = 64)]
        region_mib: u64,
        /// Where to write the snapshot file. Removed after the run by
        /// default unless `--keep` is set (TODO).
        #[arg(long, default_value = "/tmp/forkd-wp-bench.snapshot")]
        snapshot: PathBuf,
    },
}

#[derive(Subcommand)]
enum WorkspaceAction {
    /// Create a new workspace by spawning a sandbox from a snapshot
    /// tag. The workspace tracks the sandbox so future `suspend` /
    /// `resume` calls operate on it by name.
    Create {
        /// Workspace name. 1-64 chars, ASCII alnum / dash / underscore.
        name: String,
        /// Snapshot tag to fork from.
        #[arg(long)]
        snapshot: String,
        /// Place the live sandbox in its own pre-provisioned netns.
        #[arg(long)]
        per_child_netns: bool,
        /// Cgroup memory.max for the live sandbox (MiB).
        #[arg(long)]
        memory_limit_mib: Option<u64>,
        /// Controller URL. Defaults to FORKD_URL or http://127.0.0.1:8889.
        #[arg(long, env = "FORKD_URL", default_value = "http://127.0.0.1:8889")]
        daemon_url: String,
        /// Controller bearer token.
        #[arg(long, env = "FORKD_TOKEN")]
        daemon_token: Option<String>,
    },
    /// Snapshot the workspace's live sandbox and kill it. State is
    /// preserved under `ws-<name>-state`; a subsequent `resume`
    /// brings the workspace back from there.
    Suspend {
        name: String,
        /// Use v0.3 diff snapshot mode for the suspend write.
        /// ~200 ms source pause vs seconds for Full.
        #[arg(long)]
        diff: bool,
        #[arg(long, env = "FORKD_URL", default_value = "http://127.0.0.1:8889")]
        daemon_url: String,
        #[arg(long, env = "FORKD_TOKEN")]
        daemon_token: Option<String>,
    },
    /// Restore the workspace from its suspended state.
    Resume {
        name: String,
        #[arg(long, env = "FORKD_URL", default_value = "http://127.0.0.1:8889")]
        daemon_url: String,
        #[arg(long, env = "FORKD_TOKEN")]
        daemon_token: Option<String>,
    },
    /// List all workspaces tracked by the daemon.
    List {
        #[arg(long, env = "FORKD_URL", default_value = "http://127.0.0.1:8889")]
        daemon_url: String,
        #[arg(long, env = "FORKD_TOKEN")]
        daemon_token: Option<String>,
    },
    /// Delete a workspace. Kills the live sandbox (if any) and
    /// removes the state snapshot. Does NOT touch the source snapshot.
    Delete {
        name: String,
        #[arg(long, env = "FORKD_URL", default_value = "http://127.0.0.1:8889")]
        daemon_url: String,
        #[arg(long, env = "FORKD_TOKEN")]
        daemon_token: Option<String>,
    },
}

#[derive(Subcommand)]
enum ParentAction {
    /// Convert a Docker image into a writable ext4 rootfs.
    Build {
        /// Docker image (e.g. `python:3.12-slim`, `ubuntu:24.04`).
        image: String,
        /// Output ext4 file (default: `./<image-slug>.ext4`).
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
            diff,
            live,
            no_wait,
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
            diff,
            live,
            no_wait,
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
        Cmd::SnapshotDiff {
            from,
            tag,
            exec,
            exec_timeout_secs,
            daemon_url,
            daemon_token,
        } => snapshot_diff_cmd(
            &daemon_url,
            daemon_token,
            &from,
            &tag,
            &exec,
            exec_timeout_secs,
        ),
        Cmd::Fork {
            tag,
            n,
            settle_secs,
            per_child_netns,
            memory_limit_mib,
            live_fork,
            hugepages,
            keep_workdir,
        } => fork_cmd(
            tag,
            n,
            settle_secs,
            per_child_netns,
            memory_limit_mib,
            live_fork,
            hugepages,
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
        Cmd::Quickstart { n, image, yes } => quickstart_cmd(n, image, yes),
        Cmd::Run {
            image,
            extra,
            cache,
            kernel,
            tap,
            cmd,
        } => run_cmd(image, extra, cache, kernel, tap, cmd),
        Cmd::FromImage {
            image,
            tag,
            extra,
            size_mib,
            cache,
            kernel,
            tap,
            boot_wait_secs,
            mem_size_mib,
        } => from_image_cmd(
            image,
            tag,
            extra,
            size_mib,
            cache,
            kernel,
            tap,
            boot_wait_secs,
            mem_size_mib,
        ),
        Cmd::Rmi {
            tags,
            cascade,
            force,
            daemon_url,
            daemon_token,
        } => rmi_cmd(&daemon_url, daemon_token, tags, cascade, force),
        Cmd::SnapshotInfo {
            tag,
            json,
            daemon_url,
            daemon_token,
        } => snapshot_info_cmd(&daemon_url, daemon_token, &tag, json),
        Cmd::SnapshotCompact {
            from,
            to,
            daemon_url,
            daemon_token,
        } => snapshot_compact_cmd(&daemon_url, daemon_token, &from, &to),
        Cmd::Ls {
            daemon_url,
            daemon_token,
        } => sandbox::ls(&daemon_url, daemon_token),
        Cmd::Kill {
            ids,
            all,
            tag,
            daemon_url,
            daemon_token,
        } => sandbox::kill(&daemon_url, daemon_token, ids, all, tag),
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
        Cmd::Doctor {
            daemon_url,
            daemon_token,
        } => doctor::run(&daemon_url, daemon_token),
        Cmd::Bench {
            tag,
            n,
            per_child_netns,
            daemon_url,
            daemon_token,
        } => bench::run(&daemon_url, daemon_token, tag, n, per_child_netns),
        Cmd::Cleanup { yes } => cleanup_cmd(yes),
        Cmd::Push {
            tag,
            url,
            description,
            base_image,
        } => push_cmd(tag, url, description, base_image),
        Cmd::Workspace(action) => workspace_cmd(action),
        Cmd::WpBench {
            region_mib,
            snapshot,
        } => wp_bench::run(region_mib, snapshot),
    }
}

fn workspace_cmd(action: WorkspaceAction) -> Result<()> {
    use serde_json::{json, Value};
    fn daemon_request(
        method: &str,
        url: String,
        path: &str,
        token: Option<String>,
        body: Option<Value>,
    ) -> Result<Value> {
        let mut req = ureq::request(method, &format!("{}{path}", url.trim_end_matches('/')));
        if let Some(t) = token {
            req = req.set("Authorization", &format!("Bearer {t}"));
        }
        req = req.set("Content-Type", "application/json");
        let resp = match body {
            Some(b) => {
                let bytes =
                    serde_json::to_vec(&b).map_err(|e| anyhow::anyhow!("serialize body: {e}"))?;
                req.send_bytes(&bytes)
            }
            None => req.call(),
        };
        match resp {
            Ok(r) => {
                let text = r.into_string().unwrap_or_default();
                if text.is_empty() {
                    Ok(Value::Null)
                } else {
                    serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("parse response: {e}"))
                }
            }
            Err(ureq::Error::Status(code, r)) => {
                let body = r.into_string().unwrap_or_default();
                anyhow::bail!("daemon HTTP {code}: {body}")
            }
            Err(e) => Err(anyhow::anyhow!("daemon request failed: {e}")),
        }
    }
    fn print_ws(v: &Value) {
        println!(
            "{:<24} {:<10} source={:<24} state={:<24} live={}",
            v.get("name").and_then(Value::as_str).unwrap_or("?"),
            v.get("status").and_then(Value::as_str).unwrap_or("?"),
            v.get("source_snapshot_tag")
                .and_then(Value::as_str)
                .unwrap_or("?"),
            v.get("current_state_tag")
                .and_then(Value::as_str)
                .unwrap_or("-"),
            v.get("live_sandbox_id")
                .and_then(Value::as_str)
                .unwrap_or("-"),
        );
    }
    match action {
        WorkspaceAction::Create {
            name,
            snapshot,
            per_child_netns,
            memory_limit_mib,
            daemon_url,
            daemon_token,
        } => {
            let mut body = json!({
                "name": name,
                "snapshot_tag": snapshot,
                "per_child_netns": per_child_netns,
            });
            if let Some(m) = memory_limit_mib {
                body["memory_limit_mib"] = json!(m);
            }
            let resp = daemon_request(
                "POST",
                daemon_url,
                "/v1/workspaces",
                daemon_token,
                Some(body),
            )?;
            print_ws(&resp);
            Ok(())
        }
        WorkspaceAction::Suspend {
            name,
            diff,
            daemon_url,
            daemon_token,
        } => {
            let body = json!({"diff": diff});
            let resp = daemon_request(
                "POST",
                daemon_url,
                &format!("/v1/workspaces/{name}/suspend"),
                daemon_token,
                Some(body),
            )?;
            print_ws(&resp);
            Ok(())
        }
        WorkspaceAction::Resume {
            name,
            daemon_url,
            daemon_token,
        } => {
            let resp = daemon_request(
                "POST",
                daemon_url,
                &format!("/v1/workspaces/{name}/resume"),
                daemon_token,
                Some(json!({})),
            )?;
            print_ws(&resp);
            Ok(())
        }
        WorkspaceAction::List {
            daemon_url,
            daemon_token,
        } => {
            let resp = daemon_request("GET", daemon_url, "/v1/workspaces", daemon_token, None)?;
            if let Some(arr) = resp.as_array() {
                if arr.is_empty() {
                    println!("(no workspaces)");
                } else {
                    for ws in arr {
                        print_ws(ws);
                    }
                }
            } else {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&resp).unwrap_or_default()
                );
            }
            Ok(())
        }
        WorkspaceAction::Delete {
            name,
            daemon_url,
            daemon_token,
        } => {
            daemon_request(
                "DELETE",
                daemon_url,
                &format!("/v1/workspaces/{name}"),
                daemon_token,
                None,
            )?;
            println!("deleted workspace '{name}'");
            Ok(())
        }
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
    // v0.5: chain (v2) packs declare files under `chain[].files` rather
    // than the top-level `files`. Sum both so the compression-ratio
    // line stays meaningful regardless of layout.
    let total_uncompressed: u64 = manifest.files.iter().map(|f| f.size).sum::<u64>()
        + manifest
            .chain
            .iter()
            .flat_map(|c| c.files.iter())
            .map(|f| f.size)
            .sum::<u64>();
    let chain_note = if manifest.chain.is_empty() {
        String::new()
    } else {
        format!("  ({} chain links)", manifest.chain.len())
    };
    eprintln!(
        "✓ wrote {} ({} uncompressed; {:.1}× compression){chain_note} in {:.1}s",
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

    if !manifest.chain.is_empty() {
        return unpack_chain_into(tmp, manifest, tag, force);
    }

    // v1 layout (legacy single-snapshot pack). Validate the
    // manifest's declared tag *before* trusting it for path
    // computation. A pack uploaded by an attacker could declare
    // `tag = "../../etc/whatever"`; without this check, snapshot_dir()
    // would compute a path escape because Path::join silently keeps
    // the right side when it's absolute.
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
    // The staging-dir paths hub::unpack wrote into snapshot.json went
    // stale with the rename — point them at the final location.
    hub::rewrite_snapshot_paths(&dest)?;
    eprintln!("✓ unpacked tag '{final_tag}' at {}", dest.display());
    eprintln!("  next: forkd fork --tag {final_tag} -n <N>");
    Ok(())
}

/// v0.5 Phase 3: materialize a v2 chain pack — every link in
/// `manifest.chain` lives under `<tmp>/<link.tag>/` and needs to be
/// renamed into `snapshot_dir(link.tag)`.
///
/// `tag` (the `--tag` override) is only honored when the bundle
/// contains a single link (i.e. effectively a v1-shape pack written
/// in v2 wrapping). For multi-link chains the override is ambiguous
/// — which link gets re-tagged? — so we error.
///
/// `force` applies to every link uniformly: if any destination
/// already exists and `force` is unset, error out *before* touching
/// any of them so a partial unpack doesn't leave half-overwritten
/// snapshots behind.
fn unpack_chain_into(
    tmp: &std::path::Path,
    manifest: hub::Manifest,
    tag: Option<String>,
    force: bool,
) -> Result<()> {
    if tag.is_some() && manifest.chain.len() > 1 {
        bail!(
            "--tag override is not supported for multi-link chain packs \
             (this pack contains {} chained snapshots: [{}]). \
             Drop --tag to use the manifest's tags.",
            manifest.chain.len(),
            manifest
                .chain
                .iter()
                .map(|c| c.tag.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    // Resolve every destination tag up front. Single-link bundles get
    // the --tag override; multi-link chains use the manifest tags
    // verbatim (already safety-checked by hub::unpack).
    let final_tags: Vec<String> = if manifest.chain.len() == 1 && tag.is_some() {
        vec![tag.clone().unwrap()]
    } else {
        manifest.chain.iter().map(|c| c.tag.clone()).collect()
    };
    for ft in &final_tags {
        validate_tag(ft).map_err(|e| {
            anyhow::anyhow!(
                "chain pack declares an invalid tag {ft:?}: {e}. \
                 Re-pack with safer tag names."
            )
        })?;
    }

    // Pre-flight: every destination must either not exist OR force is
    // set. We check *all* of them before renaming any so we don't get
    // a half-unpacked chain on a conflict midway.
    let mut destinations: Vec<std::path::PathBuf> = Vec::with_capacity(final_tags.len());
    for ft in &final_tags {
        let dest = snapshot_dir(ft);
        if dest.exists() && !force {
            bail!(
                "tag '{ft}' already exists at {} — pass --force to \
                 overwrite all {} chain links in the bundle",
                dest.display(),
                final_tags.len()
            );
        }
        destinations.push(dest);
    }

    // Walk root → head so a partial failure leaves usable ancestors
    // behind (the user can rerun unpack on the same bundle to retry
    // the remaining links).
    for (link, (final_tag, dest)) in manifest
        .chain
        .iter()
        .zip(final_tags.iter().zip(destinations.iter()))
    {
        if dest.exists() {
            std::fs::remove_dir_all(dest)
                .with_context(|| format!("remove existing {}", dest.display()))?;
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let src = tmp.join(&link.tag);
        std::fs::rename(&src, dest)
            .with_context(|| format!("move {} → {} (chain link)", src.display(), dest.display()))?;
        // Re-point snapshot.json at the post-rename location (the
        // in-unpack pass wrote staging-dir paths).
        hub::rewrite_snapshot_paths(dest)?;
        eprintln!(
            "  ✓ link '{}' → '{final_tag}' at {}{}",
            link.tag,
            dest.display(),
            match (&link.parent_tag, &link.parent_content_hash) {
                (Some(pt), Some(_)) => format!("  (chain parent: {pt})"),
                _ => String::new(),
            }
        );
    }

    eprintln!(
        "✓ unpacked {} chain link(s), head = '{}'",
        manifest.chain.len(),
        final_tags.last().cloned().unwrap_or_default()
    );
    eprintln!(
        "  next: forkd fork --tag {} -n <N>",
        final_tags.last().cloned().unwrap_or_default()
    );
    Ok(())
}

/// Where `forkd pull <owner>/<name>` resolves names to download URLs by
/// default. Points at the registry.json maintained in the main repo.
/// Override with `--hub <url>` or `FORKD_HUB_URL` if you run your own
/// registry.
const DEFAULT_HUB_REGISTRY_URL: &str =
    "https://raw.githubusercontent.com/deeplethe/forkd/main/registry.json";

#[derive(serde::Deserialize)]
struct Registry {
    #[allow(dead_code)]
    schema_version: u32,
    packages: std::collections::HashMap<String, RegistryPackage>,
}

#[derive(serde::Deserialize)]
struct RegistryPackage {
    #[allow(dead_code)]
    description: Option<String>,
    versions: std::collections::HashMap<String, RegistryVersion>,
}

#[derive(serde::Deserialize)]
struct RegistryVersion {
    url: String,
    /// Hex-encoded SHA-256 of the pack. Verified after download; mismatch aborts.
    #[serde(default)]
    sha256: Option<String>,
    #[allow(dead_code)]
    #[serde(default)]
    size_bytes: Option<u64>,
}

fn pull_cmd(target: String, tag: Option<String>, force: bool, hub: Option<String>) -> Result<()> {
    if let Some(ref t) = tag {
        validate_tag(t)?;
    }
    let (url, expected_sha256) = resolve_target(&target, hub.as_deref())?;
    let tmp_pack = std::env::temp_dir().join(format!("forkd-pull-{}.tar.zst", std::process::id()));
    // Clean tmp_pack whether download or unpack fails — both paths used
    // to leak /tmp/forkd-pull-<pid>.tar.zst on error.
    let result = (|| -> Result<()> {
        let bytes = hub::download(&url, &tmp_pack)?;
        eprintln!("✓ downloaded {} ({})", hub::human_bytes(bytes), url);
        if let Some(expected) = expected_sha256 {
            let actual = hub::sha256_file(&tmp_pack)?;
            if !actual.eq_ignore_ascii_case(&expected) {
                bail!(
                    "sha256 mismatch — registry says {expected}, downloaded file is {actual}.\n\
                     Refusing to unpack. This means either the registry is stale or the \
                     download was tampered with."
                );
            }
            eprintln!("✓ sha256 verified ({})", &actual[..16]);
        }
        unpack_cmd(tmp_pack.clone(), tag, force)
    })();
    let _ = std::fs::remove_file(&tmp_pack);
    result
}

/// Resolve a pull target. Returns (download_url, optional_expected_sha256).
///
/// - HTTP(S) URL: passed through unchanged, no integrity check
/// - `<owner>/<name>` or `<owner>/<name>@<version>`: looked up in the
///   registry.json at `hub_base` (or `DEFAULT_HUB_REGISTRY_URL`). Picks
///   the "latest" version if `@<version>` is absent.
fn resolve_target(target: &str, hub_base: Option<&str>) -> Result<(String, Option<String>)> {
    if target.starts_with("http://") || target.starts_with("https://") {
        return Ok((target.to_string(), None));
    }
    if target.contains('/') && !target.contains(' ') {
        let (name, version) = match target.split_once('@') {
            Some((n, v)) => (n.to_string(), v.to_string()),
            None => (target.to_string(), "latest".to_string()),
        };
        let registry_url = hub_base.unwrap_or(DEFAULT_HUB_REGISTRY_URL);
        let registry = fetch_registry(registry_url)?;
        let pkg = registry.packages.get(&name).ok_or_else(|| {
            anyhow::anyhow!(
                "package '{name}' not in registry at {registry_url}. \
                 Run `curl {registry_url}` to see what's available."
            )
        })?;
        let ver = pkg.versions.get(&version).ok_or_else(|| {
            let avail: Vec<&String> = pkg.versions.keys().collect();
            anyhow::anyhow!("package '{name}' has no version '{version}'. Available: {avail:?}")
        })?;
        return Ok((ver.url.clone(), ver.sha256.clone()));
    }
    bail!("invalid pull target '{target}'; expected an https URL or `<owner>/<name>[@<version>]` short form")
}

fn fetch_registry(url: &str) -> Result<Registry> {
    eprintln!("→ resolving via {url}");
    let tmp = std::env::temp_dir().join(format!("forkd-registry-{}.json", std::process::id()));
    let _ = hub::download(url, &tmp).with_context(|| format!("fetch registry {url}"))?;
    let raw = std::fs::read_to_string(&tmp).with_context(|| "read downloaded registry")?;
    let _ = std::fs::remove_file(&tmp);
    serde_json::from_str(&raw).with_context(|| "parse registry.json")
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

/// `forkd rmi <tag>...` — delete snapshots. Daemon-first; falls back
/// to direct disk removal when the daemon is unreachable.
fn rmi_cmd(
    daemon_url: &str,
    token: Option<String>,
    tags: Vec<String>,
    cascade: bool,
    force: bool,
) -> Result<()> {
    if tags.is_empty() {
        bail!("no tags provided. Usage: forkd rmi <TAG>...");
    }
    if cascade && force {
        bail!("--cascade and --force are mutually exclusive (cascade deletes children, force orphans them)");
    }
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(30))
        .build();
    let snapshots_root = data_dir().join("snapshots");
    let mut errs = 0usize;

    for tag in &tags {
        let result = (|| -> Result<&'static str> {
            // 1. Try daemon DELETE first, threading cascade / force as
            //    query params (v0.5 Phase 4 chain-aware safety lives on
            //    the daemon side).
            let mut url = format!("{}/v1/snapshots/{}", daemon_url.trim_end_matches('/'), tag);
            let mut params: Vec<&str> = Vec::new();
            if cascade {
                params.push("cascade=true");
            }
            if force {
                params.push("force=true");
            }
            if !params.is_empty() {
                url.push('?');
                url.push_str(&params.join("&"));
            }
            let mut req = agent.delete(&url);
            if let Some(t) = token.as_deref() {
                req = req.set("Authorization", &format!("Bearer {t}"));
            }
            match req.call() {
                Ok(_) => Ok("daemon"),
                Err(ureq::Error::Status(404, _)) => {
                    // Daemon doesn't know it; try disk fallback.
                    fallback_remove(&snapshots_root, tag)?;
                    Ok("disk")
                }
                Err(ureq::Error::Status(code, r)) => {
                    let body = r.into_string().unwrap_or_default();
                    bail!("daemon HTTP {code}: {body}");
                }
                Err(_transport) => {
                    // Daemon down → disk fallback.
                    fallback_remove(&snapshots_root, tag)?;
                    Ok("disk (daemon unreachable)")
                }
            }
        })();
        match result {
            Ok(src) => println!("  ✓ {tag}  ({src})"),
            Err(e) => {
                println!("  ✗ {tag}  ({e})");
                errs += 1;
            }
        }
    }
    if errs > 0 {
        bail!("{errs} of {} removals failed", tags.len());
    }
    Ok(())
}

fn fallback_remove(snapshots_root: &std::path::Path, tag: &str) -> Result<()> {
    // Validate tag against the same rules the daemon enforces.
    if tag.is_empty()
        || tag.len() > 64
        || !tag
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("invalid tag (must be 1-64 chars, alnum / dash / underscore)");
    }
    let dir = snapshots_root.join(tag);
    if !dir.exists() {
        bail!(
            "snapshot {tag} not found (no daemon entry, no disk dir at {})",
            dir.display()
        );
    }
    std::fs::remove_dir_all(&dir).with_context(|| format!("rm -rf {}", dir.display()))?;
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
    let tag_w = infos.iter().map(|i| i.tag.len()).max().unwrap_or(8).max(3);
    println!(
        "  {:<tag_w$}  {:>10}  {:>10}  {:>10}  ROOTFS",
        "TAG",
        "SIZE",
        "MEMORY",
        "CREATED",
        tag_w = tag_w,
    );
    let mut total_bytes: u64 = 0;
    for info in &infos {
        println!(
            "  {:<tag_w$}  {:>10}  {:>10}  {:>10}  {}",
            info.tag,
            hub::human_bytes(info.total_bytes),
            hub::human_bytes(info.memory_bytes),
            hub::human_age(info.created_at_unix),
            if info.has_rootfs { "yes" } else { "—" },
            tag_w = tag_w,
        );
        total_bytes += info.total_bytes;
    }
    println!(
        "\n  {} snapshot{} · {} total",
        infos.len(),
        if infos.len() == 1 { "" } else { "s" },
        hub::human_bytes(total_bytes),
    );
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

// ----------------------------------------------------------------------
// quickstart
// ----------------------------------------------------------------------

/// Embedded copies of the host-setup scripts so `forkd quickstart`
/// works from a tarball install where the repo's `scripts/` directory
/// isn't on disk. `include_str!` keeps them in lockstep with the repo
/// copies at compile time — there is exactly one source of truth.
const QS_INSTALL_KERNEL_SH: &str = include_str!("../../../scripts/install-guest-kernel.sh");
const QS_HOST_TAP_SH: &str = include_str!("../../../scripts/host-tap.sh");
const QS_NETNS_SETUP_SH: &str = include_str!("../../../scripts/netns-setup.sh");
const QS_BUILD_ROOTFS_SH: &str = include_str!("../../../scripts/build-rootfs.sh");
const QS_FORKD_INIT_SH: &str = include_str!("../../../rootfs-init/forkd-init.sh");
const QS_FORKD_AGENT_PY: &str = include_str!("../../../rootfs-init/forkd-agent.py");

/// Make `find_script` succeed on tarball installs that have no repo
/// checkout: stage the embedded helper scripts on disk and point
/// `FORKD_SCRIPTS_DIR` at them — unless the variable is already set or
/// the script resolves on its own (repo checkouts and packaged layouts
/// keep winning).
///
/// Layout matters: `build-rootfs.sh` finds the guest init + agent via
/// `$(dirname "$0")/../rootfs-init`, so the scripts go in `<base>/scripts/`
/// and the init files in the sibling `<base>/rootfs-init/`. Get this
/// wrong and the guest boots with no init → kernel panic (error -2) at
/// warmup, long before the first fork.
fn ensure_scripts_dir_for_quickstart() -> Result<()> {
    if find_script("build-rootfs.sh").is_ok() {
        return Ok(());
    }
    let base = std::env::temp_dir().join(format!("forkd-quickstart-{}", std::process::id()));
    let scripts = base.join("scripts");
    let rootfs_init = base.join("rootfs-init");
    std::fs::create_dir_all(&scripts).context("create embedded scripts/ dir")?;
    std::fs::create_dir_all(&rootfs_init).context("create embedded rootfs-init/ dir")?;
    for (name, body) in [
        ("build-rootfs.sh", QS_BUILD_ROOTFS_SH),
        ("install-guest-kernel.sh", QS_INSTALL_KERNEL_SH),
        ("host-tap.sh", QS_HOST_TAP_SH),
        ("netns-setup.sh", QS_NETNS_SETUP_SH),
    ] {
        std::fs::write(scripts.join(name), body)
            .with_context(|| format!("write embedded {name}"))?;
    }
    for (name, body) in [
        ("forkd-init.sh", QS_FORKD_INIT_SH),
        ("forkd-agent.py", QS_FORKD_AGENT_PY),
    ] {
        std::fs::write(rootfs_init.join(name), body)
            .with_context(|| format!("write embedded {name}"))?;
    }
    std::env::set_var("FORKD_SCRIPTS_DIR", &scripts);
    Ok(())
}

fn quickstart_cmd(n: usize, image: String, yes: bool) -> Result<()> {
    eprintln!("==> forkd quickstart — host preflight");
    let pf = doctor::preflight()?;
    eprintln!();

    // Collect the setup steps this host still needs. Each is one of
    // the repo's idempotent root-gated scripts, embedded above.
    let mut fixes: Vec<(String, &str, Vec<String>)> = Vec::new();
    if !pf.kernel_ok {
        fixes.push((
            "download guest kernel vmlinux-6.1.141 (~40 MiB → /var/lib/forkd/kernels/)".into(),
            QS_INSTALL_KERNEL_SH,
            vec![],
        ));
    }
    if !pf.tap_ok {
        fixes.push((
            "create host tap forkd-tap0 (10.42.0.1/24) + enable ip_forward".into(),
            QS_HOST_TAP_SH,
            vec![],
        ));
    }
    let have_ns = doctor::netns_count();
    if have_ns < n {
        fixes.push((
            format!(
                "provision {n} per-child netns + forkd-br0 bridge + MASQUERADE \
                 ({have_ns} present)"
            ),
            QS_NETNS_SETUP_SH,
            vec![n.to_string()],
        ));
    }

    if !fixes.is_empty() {
        ensure_root("quickstart needs root for host setup; run: sudo -E forkd quickstart")?;
        eprintln!("==> quickstart wants to set up:");
        for (label, ..) in &fixes {
            eprintln!("      • {label}");
        }
        if !yes && !confirm("    proceed? [y/N] ")? {
            bail!(
                "aborted — nothing was changed. Re-run with --yes, or run the \
                 equivalent scripts/ steps by hand"
            );
        }
        for (label, script, args) in &fixes {
            eprintln!("==> {label}");
            run_embedded_script(script, args)?;
        }
        eprintln!();
    }

    // Snapshot: reuse > local Docker bake > hub pull. The local bake is
    // preferred because it uses *this host's* Firecracker, so the
    // vmstate can never hit cross-version restore errors — the exact
    // failure mode hub-distributed snapshots are exposed to.
    let tag = "quickstart".to_string();
    if snapshot_dir(&tag).join("vmstate").exists() {
        eprintln!("==> reusing existing snapshot '{tag}'");
    } else if pf.docker_ok {
        eprintln!("==> baking snapshot '{tag}' from {image} (first run 1-3 min; rootfs is cached)");
        ensure_scripts_dir_for_quickstart()?;
        from_image_cmd(
            image,
            tag.clone(),
            vec![],
            1536,
            PathBuf::from("/var/cache/forkd"),
            None,
            "forkd-tap0".to_string(),
            10,
            None,
        )?;
    } else {
        eprintln!("==> Docker unavailable — pulling a prebaked snapshot from the hub");
        eprintln!(
            "    note: hub snapshots are baked against a specific Firecracker; if the \
             restore below fails, install Docker and re-run quickstart for a local bake."
        );
        pull_cmd(
            "deeplethe/python-numpy".to_string(),
            Some(tag.clone()),
            false,
            None,
        )?;
    }
    eprintln!();

    ensure_root("forking with per-child netns needs root; run: sudo -E forkd quickstart")?;
    eprintln!("==> forking {n} children from '{tag}'");
    fork_cmd(tag.clone(), n, 2, true, None, false, false, false)?;

    eprintln!();
    eprintln!("✓ quickstart complete — {n} microVMs forked from one warm snapshot.");
    eprintln!();
    eprintln!("  where to go next:");
    eprintln!("    sudo -E forkd fork --tag {tag} -n 100 --per-child-netns    # go wider");
    eprintln!(
        "    sudo -E forkd exec --child forkd-child-1 -- python3 -c 'print(\"hi from a fork\")'"
    );
    eprintln!("    forkd images                                # snapshots on disk");
    eprintln!("    https://github.com/deeplethe/forkd/tree/main/recipes        # CI fan-out, DB fixtures, agent sandboxes");
    Ok(())
}

/// Bail with `msg` unless we're already root. The setup scripts and
/// netns-scoped forks both need it; checking up front beats a partial
/// run that dies halfway with a scarier error.
fn ensure_root(msg: &str) -> Result<()> {
    #[cfg(unix)]
    {
        if unsafe { libc::geteuid() } != 0 {
            bail!("{msg}");
        }
    }
    Ok(())
}

/// Prompt on stderr, read one stdin line, accept y/Y/yes.
fn confirm(prompt: &str) -> Result<bool> {
    use std::io::{BufRead, Write};
    eprint!("{prompt}");
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    let t = line.trim().to_ascii_lowercase();
    Ok(t == "y" || t == "yes")
}

/// Write an embedded setup script to a temp file and run it via bash.
/// stdout/stderr inherit so the operator sees exactly what it does.
fn run_embedded_script(body: &str, args: &[String]) -> Result<()> {
    let tmp = std::env::temp_dir().join(format!("forkd-quickstart-{}.sh", std::process::id()));
    std::fs::write(&tmp, body).context("write embedded setup script")?;
    let status = std::process::Command::new("bash")
        .arg(&tmp)
        .args(args)
        .status()
        .context("spawn bash for embedded setup script")?;
    std::fs::remove_file(&tmp).ok();
    if !status.success() {
        bail!("setup script failed ({status}); host may be partially configured");
    }
    Ok(())
}

/// `forkd run` — one-shot sandbox: build (if needed) → snapshot → fork → exec → kill.
/// `forkd from-image` — Docker image → ext4 (cached) → snapshot tag.
///
/// One-shot pipeline for "give me a forkd snapshot from this image".
/// The output is a registered tag you can immediately fork from.
#[allow(clippy::too_many_arguments)] // mirrors the CLI flag surface 1-to-1
fn from_image_cmd(
    image: String,
    tag: String,
    extra: Vec<String>,
    size_mib: u32,
    cache: PathBuf,
    kernel: Option<PathBuf>,
    tap: String,
    boot_wait_secs: u64,
    mem_size_mib: Option<u32>,
) -> Result<()> {
    // 1. Resolve kernel — explicit > env > standard search paths.
    let kernel = match kernel {
        Some(k) if k.exists() => k,
        Some(k) => bail!("kernel not found: {}", k.display()),
        None => find_default_kernel().ok_or_else(|| {
            anyhow::anyhow!(
                "no kernel found; pass --kernel or set FORKD_KERNEL. \
                 searched: ./vmlinux-6.1.141, ./vmlinux, /var/lib/forkd/kernels/vmlinux, \
                 /usr/local/share/forkd/vmlinux"
            )
        })?,
    };

    // 2. Materialize rootfs (cached). Same slug rule as forkd run.
    std::fs::create_dir_all(&cache).ok();
    let slug: String = image
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string();
    let rootfs = cache.join(format!("{slug}.ext4"));
    if !rootfs.exists() {
        eprintln!("==> building rootfs for {image}");
        parent_build_cmd(image.clone(), Some(rootfs.clone()), size_mib, extra)?;
    } else {
        eprintln!("==> using cached rootfs {}", rootfs.display());
    }

    // 3. Snapshot. snapshot_cmd boots the parent VM, warms it up,
    // pauses, writes memory.bin + vmstate.json under the tag dir, and
    // registers the tag with the daemon if one is running.
    eprintln!("==> snapshot --tag {tag}");
    snapshot_cmd(
        Some(tag.clone()),
        None,                                // from_sandbox (local-boot path)
        false,                               // diff (Full snapshot for new image)
        false,                               // live (Full only on local-boot)
        false,                               // no_wait (sync only on local-boot)
        "http://127.0.0.1:8889".to_string(), // daemon_url (unused on local-boot)
        None,                                // daemon_token (unused on local-boot)
        Some(kernel),
        Some(rootfs),
        true, // rw (the rootfs is .ext4)
        Some(tap),
        boot_wait_secs,
        mem_size_mib,
        false,      // keep_workdir
        Vec::new(), // volume_specs
    )?;

    eprintln!("✓ snapshot \x1b[1m{tag}\x1b[0m ready.");
    eprintln!("  next: sudo -E forkd fork --tag {tag} -n N --per-child-netns");
    Ok(())
}

/// Search standard locations for a kernel image. Returns the first that exists.
fn find_default_kernel() -> Option<PathBuf> {
    for c in [
        "./vmlinux-6.1.141",
        "./vmlinux",
        "/var/lib/forkd/kernels/vmlinux",
        "/usr/local/share/forkd/vmlinux",
    ] {
        let p = PathBuf::from(c);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

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
        false,                               // diff (unused in local-boot path)
        false,                               // live (unused in local-boot path)
        false,                               // no_wait (unused in local-boot path)
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
                netns_offset: 0,
                prewarm_scratch_dir: None,
                memory_backend: forkd_vmm::MemoryBackend::File,
                enable_diff_snapshots: false,
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
    diff: bool,
    live: bool,
    no_wait: bool,
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
        return branch_snapshot_via_daemon(
            &daemon_url,
            daemon_token,
            &sandbox_id,
            tag,
            diff,
            live,
            no_wait,
        );
    }
    if diff {
        bail!("--diff requires --from-sandbox; standalone snapshot is always Full");
    }
    if live {
        bail!("--live requires --from-sandbox; standalone snapshot is always Full");
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
        cfg = cfg.with_volume(v.clone()).with_context(|| {
            format!(
                "attaching volume {} → {}",
                v.host_path.display(),
                v.guest_path.display()
            )
        })?;
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
        parent_tag: None,
        parent_content_hash: None,
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
/// `forkd snapshot-diff` (v0.5 / M2.1) — orchestrate spawn → wait →
/// exec → BRANCH(diff, parent_tag=base) → kill against the daemon.
///
/// All five steps go through existing REST endpoints. The CLI owns
/// the orchestration so a controller crash mid-orchestration doesn't
/// orphan a sandbox the daemon can't see — the kill path is
/// idempotent and runs from the `defer` cleanup below.
fn snapshot_diff_cmd(
    daemon_url: &str,
    token: Option<String>,
    from: &str,
    new_tag: &str,
    exec: &str,
    exec_timeout_secs: u64,
) -> Result<()> {
    validate_tag(from)?;
    validate_tag(new_tag)?;
    let exec_args: Vec<String> = shell_split(exec)?;
    if exec_args.is_empty() {
        bail!("--exec must include a command (e.g. --exec \"pip install pandas\")");
    }

    let base = daemon_url.trim_end_matches('/').to_string();

    // Step 1: spawn one sandbox from <from>.
    eprintln!("==> POST {base}/v1/sandboxes  (spawning from `{from}`)");
    let spawn_body = serde_json::json!({
        "snapshot_tag": from,
        "n": 1,
        "per_child_netns": false,
    })
    .to_string();
    let spawn_resp = daemon_post(&base, "/v1/sandboxes", &spawn_body, token.as_deref())?;
    let spawn_json: serde_json::Value =
        serde_json::from_str(&spawn_resp).context("parse spawn response")?;
    let sandbox_id = spawn_json[0]["id"]
        .as_str()
        .context("spawn response missing [0].id")?
        .to_string();
    eprintln!("✓ sandbox {sandbox_id}");

    // The rest of the function does work that needs the sandbox kill
    // on every exit path. Wrap in a closure so the cleanup runs after
    // both Ok and Err.
    let result = (|| -> Result<()> {
        // Step 2: wait for the guest agent to respond to /ping.
        // Restore usually completes the agent socket within a few
        // hundred ms but we give up to 30 s to allow for cold-cache
        // first-spawns on large parents.
        wait_for_guest_agent(&base, &sandbox_id, token.as_deref(), 30)?;

        // Step 3: exec the installer.
        eprintln!("==> POST {base}/v1/sandboxes/{sandbox_id}/exec");
        eprintln!("    args: {exec_args:?}");
        let exec_body = serde_json::json!({
            "args": exec_args,
            "timeout_secs": exec_timeout_secs,
        })
        .to_string();
        let exec_resp = daemon_post(
            &base,
            &format!("/v1/sandboxes/{sandbox_id}/exec"),
            &exec_body,
            token.as_deref(),
        )?;
        let exec_json: serde_json::Value =
            serde_json::from_str(&exec_resp).context("parse exec response")?;
        let exit_code = exec_json["exit_code"].as_i64().unwrap_or(-1);
        if exit_code != 0 {
            let stdout = exec_json["stdout"].as_str().unwrap_or("");
            let stderr = exec_json["stderr"].as_str().unwrap_or("");
            bail!(
                "exec `{}` exited with code {exit_code}\n--- stdout ---\n{stdout}\n\
                 --- stderr ---\n{stderr}\n--- end ---",
                exec_args.join(" "),
            );
        }
        eprintln!("✓ exec exit 0");

        // Step 4: BRANCH with parent_tag set so the daemon records
        // the chain edge in the resulting snapshot.json.
        eprintln!("==> POST {base}/v1/sandboxes/{sandbox_id}/branch  (diff, parent_tag=`{from}`)");
        let branch_body = serde_json::json!({
            "tag": new_tag,
            "mode": "diff",
            "parent_tag": from,
        })
        .to_string();
        let branch_resp = daemon_post(
            &base,
            &format!("/v1/sandboxes/{sandbox_id}/branch"),
            &branch_body,
            token.as_deref(),
        )?;
        let branch_json: serde_json::Value =
            serde_json::from_str(&branch_resp).context("parse branch response")?;
        let dir = branch_json["dir"].as_str().unwrap_or("?");
        let pause_ms = branch_json["pause_ms"].as_u64().unwrap_or(0);
        let phys = branch_json["diff_physical_bytes"].as_u64().unwrap_or(0);
        eprintln!("✓ chained snapshot ready: tag=`{new_tag}` pause={pause_ms}ms diff_bytes={phys}");
        println!("tag:                  {new_tag}");
        println!("dir:                  {dir}");
        println!("parent_tag:           {from}");
        println!("pause_ms:             {pause_ms}");
        println!("diff_physical_bytes:  {phys}");
        Ok(())
    })();

    // Step 5: tear down the spawned sandbox unconditionally (idempotent —
    // 404 is fine if the daemon already removed it for some reason).
    eprintln!("==> DELETE {base}/v1/sandboxes/{sandbox_id}");
    let _ = daemon_delete(
        &base,
        &format!("/v1/sandboxes/{sandbox_id}"),
        token.as_deref(),
    );
    result
}

/// v0.5 Phase 4: `forkd snapshot-info <tag>` — GET the daemon's
/// chain + size + dependents view of one snapshot, render as a
/// human-readable table or pass-through JSON.
fn snapshot_info_cmd(
    daemon_url: &str,
    token: Option<String>,
    tag: &str,
    as_json: bool,
) -> Result<()> {
    validate_tag(tag)?;
    let base = daemon_url.trim_end_matches('/').to_string();
    let url = format!("{base}/v1/snapshots/{tag}/info");
    let mut req = ureq::get(&url);
    if let Some(t) = token.as_deref() {
        req = req.set("Authorization", &format!("Bearer {t}"));
    }
    let resp = match req.call() {
        Ok(r) => r
            .into_string()
            .context("read snapshot-info response body")?,
        Err(ureq::Error::Status(404, _)) => bail!("snapshot `{tag}` not found"),
        Err(ureq::Error::Status(code, r)) => {
            let body = r.into_string().unwrap_or_default();
            bail!("HTTP {code}: {body}");
        }
        Err(e) => bail!("HTTP GET {url} failed: {e}"),
    };
    if as_json {
        println!("{resp}");
        return Ok(());
    }
    let v: serde_json::Value = serde_json::from_str(&resp).context("parse snapshot-info JSON")?;
    let mib = |b: u64| -> String { format!("{:.1} MiB", (b as f64) / (1024.0 * 1024.0)) };
    let kib = |b: u64| -> String { format!("{:.1} KiB", (b as f64) / 1024.0) };
    println!("tag:                  {}", v["tag"].as_str().unwrap_or(tag));
    println!("dir:                  {}", v["dir"].as_str().unwrap_or("?"));
    println!(
        "chain depth:          {}",
        v["chain_depth"].as_u64().unwrap_or(0)
    );
    match v["parent_tag"].as_str() {
        Some(p) => println!("parent_tag:           {p}"),
        None => println!("parent_tag:           (none — this is a base snapshot)"),
    }
    if let Some(h) = v["parent_content_hash"].as_str() {
        println!("parent_content_hash:  {h}");
    }
    println!(
        "memory.bin:           {} logical / {} on disk",
        mib(v["memory_logical_bytes"].as_u64().unwrap_or(0)),
        mib(v["memory_physical_bytes"].as_u64().unwrap_or(0)),
    );
    println!(
        "vmstate:              {}",
        kib(v["vmstate_bytes"].as_u64().unwrap_or(0))
    );
    let ancestors = v["ancestors"].as_array().cloned().unwrap_or_default();
    if ancestors.is_empty() {
        println!("ancestors:            (none)");
    } else {
        let names: Vec<&str> = ancestors.iter().filter_map(|x| x.as_str()).collect();
        println!(
            "ancestors:            {} (root → parent)",
            names.join(" → ")
        );
    }
    let dependents = v["dependents"].as_array().cloned().unwrap_or_default();
    if dependents.is_empty() {
        println!("dependents:           (none — chain leaf or no children yet)");
    } else {
        let names: Vec<&str> = dependents.iter().filter_map(|x| x.as_str()).collect();
        println!(
            "dependents:           {} (would be orphaned by `forkd rmi {tag}` — pass --cascade or --force)",
            names.join(", ")
        );
    }
    Ok(())
}

/// v0.5 Phase 4: `forkd snapshot-compact --from <tag> --to <new>` —
/// POST to the daemon's compact endpoint. The daemon resolves the
/// chain, verifies parent hashes, assembles the memory image, copies
/// the head's vmstate, and persists a new flat snapshot.
fn snapshot_compact_cmd(
    daemon_url: &str,
    token: Option<String>,
    from: &str,
    to: &str,
) -> Result<()> {
    validate_tag(from)?;
    validate_tag(to)?;
    if from == to {
        bail!("--from and --to must differ");
    }
    let base = daemon_url.trim_end_matches('/').to_string();
    eprintln!("==> POST {base}/v1/snapshots/{from}/compact  (→ `{to}`)");
    let body = serde_json::json!({ "to": to }).to_string();
    let resp = daemon_post(
        &base,
        &format!("/v1/snapshots/{from}/compact"),
        &body,
        token.as_deref(),
    )?;
    let v: serde_json::Value = serde_json::from_str(&resp).context("parse compact response")?;
    println!("✓ compacted snapshot");
    println!("tag:                  {}", v["tag"].as_str().unwrap_or(to));
    println!("dir:                  {}", v["dir"].as_str().unwrap_or("?"));
    if let Some(prod) = v["branched_from"].as_str() {
        println!("source lineage:       {prod}");
    }
    println!("\nVerify with: forkd snapshot-info {to}");
    Ok(())
}

/// Poll `POST /v1/sandboxes/<id>/ping` until the guest agent answers
/// or `deadline_secs` elapses. Spawn returns once FC restore completes,
/// but the guest TCP listener can take a few hundred ms more to come up.
fn wait_for_guest_agent(
    base: &str,
    sandbox_id: &str,
    token: Option<&str>,
    deadline_secs: u64,
) -> Result<()> {
    let url_path = format!("/v1/sandboxes/{sandbox_id}/ping");
    let start = Instant::now();
    let deadline = Duration::from_secs(deadline_secs);
    let mut backoff = Duration::from_millis(100);
    loop {
        if start.elapsed() >= deadline {
            bail!(
                "guest agent in sandbox `{sandbox_id}` did not respond to ping within {deadline_secs}s"
            );
        }
        match daemon_post(base, &url_path, "", token) {
            Ok(_) => return Ok(()),
            Err(e) => {
                let msg = format!("{e:#}");
                // The daemon returns 500 when the ping connection
                // fails, which is the expected "guest not ready yet"
                // signal. Retry. 4xx (e.g. 404 sandbox gone) is
                // terminal.
                if msg.contains("HTTP 4") {
                    bail!("ping failed: {msg}");
                }
                thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_secs(2));
            }
        }
    }
}

/// Minimal shell-style splitter. Handles plain whitespace + single
/// and double quotes; doesn't handle escapes or `$var`. Sufficient
/// for `--exec "pip install pandas==2.0.0"` style strings.
fn shell_split(s: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for c in s.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (None, '"') | (None, '\'') => quote = Some(c),
            (None, c) if c.is_whitespace() => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            _ => cur.push(c),
        }
    }
    if quote.is_some() {
        bail!("--exec has an unterminated quote: `{s}`");
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    Ok(out)
}

/// Thin wrapper around the daemon's POST endpoints used by the
/// orchestration verb. Returns the response body on 2xx; bails with
/// `HTTP {code}: {body}` on non-2xx so the caller can pattern-match.
fn daemon_post(base: &str, path: &str, body: &str, token: Option<&str>) -> Result<String> {
    let url = format!("{base}{path}");
    let mut req = ureq::post(&url).set("Content-Type", "application/json");
    if let Some(t) = token {
        req = req.set("Authorization", &format!("Bearer {t}"));
    }
    match req.send_string(body) {
        Ok(r) => r.into_string().context("read daemon response body"),
        Err(ureq::Error::Status(code, r)) => {
            let body = r.into_string().unwrap_or_default();
            bail!("HTTP {code}: {body}");
        }
        Err(e) => bail!("HTTP POST {url} failed: {e}"),
    }
}

fn daemon_delete(base: &str, path: &str, token: Option<&str>) -> Result<()> {
    let url = format!("{base}{path}");
    let mut req = ureq::delete(&url);
    if let Some(t) = token {
        req = req.set("Authorization", &format!("Bearer {t}"));
    }
    match req.call() {
        Ok(_) => Ok(()),
        // 404 = sandbox already gone; idempotent cleanup so don't error.
        Err(ureq::Error::Status(404, _)) => Ok(()),
        Err(ureq::Error::Status(code, r)) => {
            let body = r.into_string().unwrap_or_default();
            bail!("HTTP {code}: {body}");
        }
        Err(e) => bail!("HTTP DELETE {url} failed: {e}"),
    }
}

fn branch_snapshot_via_daemon(
    daemon_url: &str,
    token: Option<String>,
    sandbox_id: &str,
    tag: Option<String>,
    diff: bool,
    live: bool,
    no_wait: bool,
) -> Result<()> {
    let url = format!(
        "{}/v1/sandboxes/{}/branch",
        daemon_url.trim_end_matches('/'),
        sandbox_id
    );
    let mut body_map = serde_json::Map::new();
    if let Some(t) = tag.as_deref() {
        validate_tag(t)?;
        body_map.insert("tag".into(), serde_json::Value::String(t.into()));
    }
    // For --diff, keep sending the legacy `diff: true` field so this
    // CLI can drive both v0.3.x and v0.4+ daemons (v0.4 still accepts
    // the bool; v0.3 doesn't know `mode`). For --live, send the
    // canonical Phase 7 `mode: "live"` — old daemons don't support
    // live BRANCH anyway, so there's no compat path to preserve.
    // clap already enforced --diff and --live are mutually exclusive.
    if diff {
        body_map.insert("diff".into(), serde_json::Value::Bool(true));
    } else if live {
        body_map.insert("mode".into(), serde_json::Value::String("live".into()));
    }
    if no_wait {
        // clap requires `--no-wait` to come with `--live`, so this
        // branch is always live mode here.
        body_map.insert("wait".into(), serde_json::Value::Bool(false));
    }
    let body = serde_json::Value::Object(body_map).to_string();
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
    if let Some(w) = info.get("warning").and_then(|v| v.as_str()) {
        eprintln!("\x1b[33m⚠ warning:\x1b[0m {w}");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn fork_cmd(
    tag: String,
    n: usize,
    settle_secs: u64,
    per_child_netns: bool,
    memory_limit_mib: Option<u64>,
    live_fork: bool,
    hugepages: bool,
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
                // CLI `forkd fork` is one-shot: it always allocates starting
                // from forkd-child-1. The daemon path picks a non-colliding
                // offset based on live state; the CLI doesn't have that view.
                netns_offset: 0,
                // CLI fork is one-shot — caller can re-run if cold matters.
                // The daemon's create_sandbox path is where prewarm pays off.
                prewarm_scratch_dir: None,
                // `--live-fork` opts each child into a per-child memfd
                // (see lib.rs Phase 1.5) so a later v0.4 live BRANCH from
                // it can arm UFFD_WP on the shmem-backed VMA. Default
                // stays File for backward compat with v0.3.x flows.
                memory_backend: if live_fork {
                    forkd_vmm::MemoryBackend::MemfdShared {
                        use_hugepages: hugepages,
                    }
                } else {
                    forkd_vmm::MemoryBackend::File
                },
                // CLI fork doesn't outlive its invocation, no diff snapshots.
                enable_diff_snapshots: false,
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

    // --- v0.5 Phase 2b: shell_split for `forkd snapshot-diff --exec` ----

    #[test]
    fn shell_split_simple_words() {
        let v = shell_split("pip install pandas").unwrap();
        assert_eq!(v, vec!["pip", "install", "pandas"]);
    }

    #[test]
    fn shell_split_collapses_runs_of_whitespace() {
        let v = shell_split("  pip   install  \t  pandas\n").unwrap();
        assert_eq!(v, vec!["pip", "install", "pandas"]);
    }

    #[test]
    fn shell_split_preserves_quoted_strings_with_spaces() {
        // The motivating use case: pip install with a version
        // specifier embedded in a quoted argument.
        let v = shell_split(r#"sh -c "pip install pandas==2.0.0""#).unwrap();
        assert_eq!(v, vec!["sh", "-c", "pip install pandas==2.0.0"]);
    }

    #[test]
    fn shell_split_handles_single_and_double_quotes() {
        let v = shell_split(r#"a 'b c' "d e" f"#).unwrap();
        assert_eq!(v, vec!["a", "b c", "d e", "f"]);
    }

    #[test]
    fn shell_split_rejects_unterminated_quote() {
        let err = shell_split(r#"pip install "pandas"#).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("unterminated quote"),
            "actionable error required; got: {msg}"
        );
    }

    #[test]
    fn shell_split_empty_input_yields_empty_vec() {
        // Reject upstream — main path bails when args.is_empty() —
        // but the splitter itself just returns []. Keeps the
        // splitter pure.
        assert!(shell_split("").unwrap().is_empty());
        assert!(shell_split("   \t\n  ").unwrap().is_empty());
    }

    #[test]
    fn parse_volume_rejects_bad_flag() {
        assert!(parse_volume("/host.img:/guest:wat").is_err());
    }
}
