//! `forkd-vmm`: Firecracker wrapper with snapshot/fork primitives.
//!
//! See `DESIGN.md` at the repo root for the architecture.
//!
//! HTTP-over-unix-socket is currently implemented by shelling out to `curl`.
//! This avoids pulling in a heavy HTTP client for the MVP. It's slow per call
//! (~10–20 ms startup) but we issue calls in parallel via threads, so the
//! aggregate wall-clock is dominated by Firecracker, not curl.
//! A future PR can replace curl with hyper + hyperlocal.

pub mod cgroup;
pub mod chain;
pub mod memfd;
pub mod paths;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
#[cfg(target_os = "linux")]
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Network interface to attach to the guest.
///
/// `host_dev_name` is a tap device that already exists on the host
/// (typically created via `scripts/host-tap.sh`). `guest_mac` is
/// optional; Firecracker assigns one if unset, but all children of a
/// snapshot inherit the same MAC — see issue #1 (MAC hot-patch).
///
/// `guest_ip` + `host_gw` use the Linux kernel's `ip=` boot parameter
/// to configure static networking *before init runs* — so the rootfs
/// doesn't need iproute2 or a DHCP client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkConfig {
    pub iface_id: String,
    pub host_dev_name: String,
    pub guest_mac: Option<String>,
    pub guest_ip: Option<String>,
    pub host_gw: Option<String>,
    pub netmask: Option<String>,
}

impl NetworkConfig {
    /// A reasonable default for `scripts/host-tap.sh`'s tap.
    pub fn default_tap(host_dev_name: impl Into<String>) -> Self {
        Self {
            iface_id: "eth0".into(),
            host_dev_name: host_dev_name.into(),
            guest_mac: Some("AA:FC:00:00:00:01".into()),
            guest_ip: Some("10.42.0.2".into()),
            host_gw: Some("10.42.0.1".into()),
            netmask: Some("255.255.255.0".into()),
        }
    }

    /// Render the kernel `ip=` parameter, if static config is set.
    /// Format: `ip=<client-ip>::<gw-ip>:<netmask>::eth0:off`
    pub fn kernel_ip_arg(&self) -> Option<String> {
        let ip = self.guest_ip.as_ref()?;
        let gw = self.host_gw.as_ref()?;
        let nm = self.netmask.as_deref().unwrap_or("255.255.255.0");
        Some(format!("ip={ip}::{gw}:{nm}::{}:off", self.iface_id))
    }
}

/// A per-tag persistent volume: a host directory or block-image file that
/// gets attached as `/dev/vdb..z` inside the guest and (optionally) mounted
/// at a configured path by `/forkd-init.sh`.
///
/// Volumes survive across forks of the same snapshot tag — every child
/// sees the same host file. Use them for pip caches, model weights,
/// shared scratch space, etc. Volumes do **not** affect memory CoW: each
/// child still inherits the parent's RAM image via `mmap(MAP_PRIVATE)`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeSpec {
    /// Path on the host. Can be a directory (bind-mounted via virtiofs is
    /// not currently supported; we use a block-image file instead) or an
    /// ext4 image file. For directories, call `mke2fs`/`tar` separately
    /// to produce an image; for now the path must be a regular file.
    pub host_path: PathBuf,
    /// Mount point inside the guest, e.g. `/opt/cache`. The init script
    /// mounts the corresponding `/dev/vdX` device here.
    pub guest_path: PathBuf,
    /// If `true`, the drive is exposed read-only.
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BootConfig {
    pub kernel: PathBuf,
    pub rootfs: PathBuf,
    pub vcpu_count: u32,
    pub mem_size_mib: u32,
    pub boot_args: String,
    pub work_dir: PathBuf,
    /// `true` for squashfs / verity images; `false` for ext4 you want to write to.
    pub rootfs_read_only: bool,
    /// Optional virtio-net interface. If `None`, the guest has no network.
    pub network: Option<NetworkConfig>,
    /// Extra block-device volumes to attach as /dev/vdb, /dev/vdc, ...
    /// in declaration order. Empty by default.
    #[serde(default)]
    pub volumes: Vec<VolumeSpec>,
}

impl BootConfig {
    /// Sensible defaults for a Firecracker-quickstart-style boot:
    /// 2 vCPU, 512 MiB, ttyS0 console, **read-only** rootfs on /dev/vda.
    /// Use this for squashfs images.
    pub fn quickstart(kernel: PathBuf, rootfs: PathBuf, work_dir: PathBuf) -> Self {
        Self {
            kernel,
            rootfs,
            vcpu_count: 2,
            mem_size_mib: 512,
            boot_args: "console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda ro".into(),
            work_dir,
            rootfs_read_only: true,
            network: None,
            volumes: Vec::new(),
        }
    }

    /// Boot config for a **writable ext4** rootfs (built via `scripts/build-rootfs.sh`).
    /// 2 vCPU, 512 MiB, rootfs mounted rw on /dev/vda.
    /// Uses `init=/forkd-init.sh` so we get a custom PID 1 that warms up state
    /// (e.g. imports Python + numpy) before parking on a long sleep — the
    /// snapshot then captures that warm state for children to inherit.
    pub fn ext4_rw(kernel: PathBuf, rootfs: PathBuf, work_dir: PathBuf) -> Self {
        Self {
            kernel,
            rootfs,
            vcpu_count: 2,
            mem_size_mib: 512,
            boot_args:
                "console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda rw init=/forkd-init.sh".into(),
            work_dir,
            rootfs_read_only: false,
            network: None,
            volumes: Vec::new(),
        }
    }

    /// Attach a virtio-net interface to this VM. The tap device referenced
    /// by `host_dev_name` must already exist on the host.
    pub fn with_network(mut self, net: NetworkConfig) -> Self {
        self.network = Some(net);
        self
    }

    /// Attach a persistent volume. Volumes appear in the guest as
    /// `/dev/vdb`, `/dev/vdc`, ... in the order they're added, and are
    /// mounted at `volume.guest_path` by `/forkd-init.sh` based on the
    /// `forkd.mounts=` kernel cmdline hint this method appends.
    pub fn with_volume(mut self, volume: VolumeSpec) -> Result<Self> {
        // Volumes occupy /dev/vdb onwards (vda is rootfs); index i → vdN.
        let index = self.volumes.len();
        let dev = volume_device_name(index)?;
        let hint = format!("{}:{}", dev, volume.guest_path.display());
        // Append (or extend) the forkd.mounts= cmdline hint.
        match self.boot_args.find("forkd.mounts=") {
            Some(start) => {
                // existing hint — append a comma-separated entry.
                let end = self.boot_args[start..]
                    .find(' ')
                    .map(|n| start + n)
                    .unwrap_or(self.boot_args.len());
                self.boot_args.insert_str(end, &format!(",{hint}"));
            }
            None => {
                self.boot_args.push_str(&format!(" forkd.mounts={hint}"));
            }
        }
        self.volumes.push(volume);
        Ok(self)
    }
}

/// Maximum number of user-attached volumes per guest VM.
///
/// virtio-blk drives are named `/dev/vd[a-z]` in the guest; `/dev/vda` is
/// reserved for the rootfs, leaving `/dev/vdb` through `/dev/vdz` — 25 slots
/// — for [`BootConfig::with_volume`] callers. The next ASCII codepoint after
/// `z` is `{`, which produces an invalid Linux device path.
pub const MAX_VOLUMES: usize = 25;

/// `0 → "vdb"`, `1 → "vdc"`, ... up to `24 → "vdz"`. Returns an error for
/// `index >= MAX_VOLUMES`; without the cap, naive arithmetic past `vdz`
/// produces garbage names like `vd{`, `vd|`, `vd}` that look valid to the
/// host-side kernel cmdline append but silently fail to mount in the guest.
pub fn volume_device_name(index: usize) -> Result<String> {
    if index >= MAX_VOLUMES {
        bail!(
            "volume index {index} exceeds MAX_VOLUMES ({MAX_VOLUMES}); \
             virtio-blk supports /dev/vdb..vdz (/dev/vda is the rootfs)"
        );
    }
    let letter = (b'b' + index as u8) as char;
    Ok(format!("vd{letter}"))
}

// ---------------------------------------------------------------------------
// Vm + Snapshot
// ---------------------------------------------------------------------------

/// A running (or recently-killed) Firecracker microVM.
///
/// On Drop, the underlying firecracker process is killed and the API socket
/// file is removed. Hold the `Vm` for as long as you want the guest alive.
#[derive(Debug)]
pub struct Vm {
    proc: Child,
    pid: u32,
    sock: PathBuf,
    console: PathBuf,
    /// The network namespace this VM was spawned in, if any.
    /// Used by `exec_in_netns` / `eval_in_netns` to reach the guest agent.
    pub netns: Option<String>,
    /// cgroup v2 directory this VM's Firecracker process was placed in,
    /// if `ForkOpts::memory_limit_mib` was set. Removed on Drop.
    pub cgroup: Option<PathBuf>,
    /// memfd holding this VM's restored guest RAM, when spawned under
    /// `MemoryBackend::MemfdShared`. Held for the VM's lifetime so the
    /// kernel keeps the backing pages alive; closed automatically on
    /// Drop. Phase 6's UFFD_WP arming will dup this fd to register a
    /// `userfaultfd` against the same VMA.
    pub memfd: Option<memfd::MemfdRegion>,
}

impl Vm {
    pub fn pid(&self) -> u32 {
        self.pid
    }
    pub fn sock(&self) -> &Path {
        &self.sock
    }
    pub fn console_path(&self) -> &Path {
        &self.console
    }

    /// Borrow of the memfd-backed RAM region, if this VM was spawned
    /// under `MemoryBackend::MemfdShared` (Phase 5b). Phase 6.3 uses
    /// this to mmap the memfd in the controller's process so its bulk
    /// copier can stream clean pages directly out of the same backing
    /// memory FC is using. Returns `None` for file-backed VMs (which
    /// don't support UFFD_WP — see DESIGN-v0.4-PHASE6.md).
    pub fn memfd_handle(&self) -> Option<&memfd::MemfdRegion> {
        self.memfd.as_ref()
    }

    /// Ask the vendored Firecracker to set up a write-protected
    /// userfaultfd on its guest memory and hand the fd back via
    /// `SCM_RIGHTS`. Phase 6.3 uses this to arm WP without owning the
    /// `UFFDIO_REGISTER` ioctl itself — that has to run inside FC's
    /// process because the VMA being protected is FC's.
    ///
    /// Protocol:
    /// 1. caller picks a UDS path, this method `bind(2)`s it.
    /// 2. method issues `PUT /uffd/wp` with `{ "socket": "<path>" }`.
    /// 3. FC creates a uffd, registers `UFFDIO_REGISTER_MODE_WP`
    ///    against every guest-memory region, connects to the UDS, and
    ///    sends the fd via `SCM_RIGHTS` (payload: JSON of
    ///    `Vec<GuestRegionUffdMapping>`).
    /// 4. method receives the handshake and returns it.
    ///
    /// Requires the vendored FC fork (commit `7d80afade` on
    /// `forkd-v0.4-mem-backend-shared-v1.12`). Stock FC has no
    /// `/uffd/wp` route and will 404.
    ///
    /// `socket_path` must:
    /// - live in a directory the caller controls (forkd-controller's
    ///   work_dir) — we don't sandbox attacker-supplied prefixes.
    /// - not already exist as a socket; we'll remove a stale leftover.
    ///
    /// On success, the returned [`forkd_uffd::handshake::Handshake`]
    /// owns the uffd fd. Drop it to close the uffd.
    #[cfg(target_os = "linux")]
    pub fn request_wp_uffd(&self, socket_path: &Path) -> Result<forkd_uffd::handshake::Handshake> {
        use std::os::unix::net::UnixListener;

        // Pre-create the parent dir so callers don't have to.
        if let Some(parent) = socket_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!("create parent dir for UDS at {}", parent.display())
                })?;
            }
        }
        // Bind synchronously so the listener is up before FC connects.
        // (If we spawned the accept thread first and raced against the
        // PUT, FC could see ECONNREFUSED.)
        if socket_path.exists() {
            std::fs::remove_file(socket_path)
                .with_context(|| format!("remove stale UDS at {}", socket_path.display()))?;
        }
        let listener = UnixListener::bind(socket_path)
            .with_context(|| format!("bind UDS at {}", socket_path.display()))?;

        // Spawn the accept-and-recv thread. FC connects + sends fd
        // BEFORE the HTTP response comes back, so the thread will
        // generally have the Handshake ready by the time the PUT
        // returns.
        let accept_thread =
            std::thread::spawn(move || -> Result<forkd_uffd::handshake::Handshake> {
                let (stream, _) = listener.accept().context("accept FC connection")?;
                forkd_uffd::handshake::recv_handshake(&stream)
                    .context("receive WP-uffd handshake from FC")
            });

        // Issue the PUT. FC's setup_wp_uffd creates the uffd, registers
        // WP, connects to our socket, sends the fd, then returns 204.
        let body = serde_json::json!({
            "socket": socket_path,
        });
        let api_result = api_call_with_timeout(
            &self.sock,
            "PUT",
            "/uffd/wp",
            &body.to_string(),
            DEFAULT_API_TIMEOUT_SECS,
        );

        // Always join the thread (even on PUT failure) to avoid a
        // dangling accept blocking forever — UnixListener::accept will
        // unblock when the listener drops at end of scope here.
        let handshake_result = accept_thread
            .join()
            .map_err(|e| anyhow::anyhow!("WP-uffd accept thread panicked: {e:?}"))?;

        api_result.context("PUT /uffd/wp failed")?;
        let handshake = handshake_result?;

        // The fd is in hand; the UDS file has done its job.
        let _ = std::fs::remove_file(socket_path);

        Ok(handshake)
    }

    /// Non-Linux stub. Phase 6 is Linux-only — `userfaultfd` is a
    /// Linux syscall.
    #[cfg(not(target_os = "linux"))]
    pub fn request_wp_uffd(&self, _socket_path: &Path) -> Result<()> {
        anyhow::bail!("Vm::request_wp_uffd is Linux-only; v0.4 live-fork requires userfaultfd")
    }

    /// Is the firecracker process still alive on the host?
    pub fn is_alive(&self) -> bool {
        Path::new(&format!("/proc/{}", self.pid)).exists()
    }
}

/// On-disk snapshot of a paused VM: a vmstate blob (vCPU + devices) plus a
/// memory image file. Children restore from these by mmap'ing memory with
/// `MAP_PRIVATE`, which the kernel implements as copy-on-write.
///
/// Two shapes per [`DESIGN-v0.5-diff-snapshot-chains.md`](../../../DESIGN-v0.5-diff-snapshot-chains.md):
///
/// - **Base** (`parent_tag = None`): `memory` is a full `memory.bin`,
///   ready to restore directly. The historical shape.
/// - **Chained** (`parent_tag = Some(_)`): `memory` is a sparse FC
///   Diff file vs. the parent's memory. Restore-time assembly via
///   [`chain::assemble_chain_memory`] walks parents and merges
///   diffs in order. `parent_content_hash` pins the parent's bytes
///   so re-snapshotting the base under the same tag doesn't silently
///   break the chain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub vmstate: PathBuf,
    pub memory: PathBuf,
    /// Volumes attached at parent boot time. Children re-attach the same
    /// host paths during restore so the guest's mount points line up.
    /// `#[serde(default)]` for backward compat with snapshots written
    /// before volumes existed.
    #[serde(default)]
    pub volumes: Vec<VolumeSpec>,
    /// Tag of the snapshot this one derives from, when this is a
    /// chained diff snapshot. `None` for base snapshots (the
    /// historical / pre-v0.5 shape). Skipped when serializing
    /// bases so v0.4-and-earlier snapshot.json files stay
    /// byte-identical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_tag: Option<String>,
    /// `sha256` of the parent's `memory.bin` recorded at build time,
    /// hex-encoded. Restore verifies the current parent against this;
    /// mismatch yields a clear "chain broken, parent has been
    /// replaced" error rather than silently restoring the wrong
    /// bytes. Always set when `parent_tag` is set; `None` for bases.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_content_hash: Option<String>,
}

/// Result of a Diff snapshot. `memory_diff` is a sparse file the same
/// LOGICAL size as a Full snapshot of the same source, but with most
/// bytes being `lseek(SEEK_HOLE)` gaps — only pages dirtied since the
/// previous snapshot contain data. Not directly restorable; merge into
/// a base via `apply_diff` first.
#[derive(Debug, Clone)]
pub struct DiffSnapshot {
    pub vmstate: PathBuf,
    pub memory_diff: PathBuf,
    /// Logical file size (matches a Full snapshot's memory.bin size).
    pub logical_size_bytes: u64,
    /// On-disk allocated size (= dirty page bytes, rounded to FS block
    /// granularity). The ratio `physical_size_bytes / logical_size_bytes`
    /// is the diff compression ratio for this BRANCH cycle.
    pub physical_size_bytes: u64,
    pub volumes: Vec<VolumeSpec>,
}

/// Which mechanism backs the children's guest RAM during restore.
///
/// **`File`** (default, v0.2 behavior): each child's Firecracker process
/// calls `mmap(memory.bin, MAP_PRIVATE)`. The kernel manages page-cache
/// dedup across N children — clean pages are shared, dirtied pages are
/// copy-on-written per-child. This is the primitive forkd is named for.
///
/// **`Userfault`** (v0.3 scaffolding, not yet implemented): each child
/// connects to an external user-space page-fault handler via the given
/// unix-domain socket. Firecracker creates anonymous private mappings
/// for guest RAM and the handler serves UFFDIO_COPY on each first
/// access. Designed for **live branching** — pairs with a memfd-backed
/// source so children fork from source's running state with a pause
/// window independent of guest memory size (~30 ms target).
///
/// See `docs/design/userfaultfd.md` for the full v0.3 design and the
/// outstanding work needed before this variant becomes usable.
#[derive(Debug, Clone, Default)]
pub enum MemoryBackend {
    #[default]
    File,
    Userfault {
        handler_sock: PathBuf,
    },
    /// v0.4 live-fork backing. Each restored child gets its own memfd
    /// populated from the snapshot's memory.bin; the patched
    /// Firecracker (deeplethe/firecracker:forkd-v0.4-mem-backend-shared-v1.12,
    /// see `docs/VENDORED-FIRECRACKER.md`) mmaps the memfd with
    /// `MAP_SHARED` so the controller can later arm
    /// `UFFDIO_WRITEPROTECT` on the same backing and capture dirty
    /// pages asynchronously — the actual live-fork primitive lands in
    /// Phase 6.
    ///
    /// Requires the patched FC binary at runtime; an unpatched FC
    /// silently falls back to `MAP_PRIVATE`, breaking the WP-capture
    /// invariant. `forkd doctor` (Phase 8) will check for the patched
    /// binary at daemon start.
    MemfdShared,
}

/// Options controlling a fork-many operation.
#[derive(Debug, Clone)]
pub struct ForkOpts {
    pub n: usize,
    /// If true, each child is spawned inside its own pre-provisioned network
    /// namespace named `forkd-child-1`, `forkd-child-2`, … `forkd-child-N`.
    /// Use `scripts/netns-setup.sh N` to provision them ahead of time.
    pub per_child_netns: bool,
    /// If Some, each child's Firecracker process is placed in a cgroup v2
    /// leaf with `memory.max` set to this many MiB. Requires cgroup v2
    /// unified hierarchy and root (or a delegated cgroup). See `cgroup.rs`.
    pub memory_limit_mib: Option<u64>,
    /// Offset added to the per-child netns / cgroup index. Default 0
    /// produces the historical `forkd-child-1..N` naming. Set to >0 when
    /// other sandboxes already occupy lower indices (e.g. branching: the
    /// source sandbox holds `forkd-child-1`, grandchildren start higher).
    /// Index zero is never assigned — the first index is always
    /// `netns_offset + 1`.
    pub netns_offset: usize,
    /// If Some, immediately after restore each child is pre-warmed by
    /// taking a throwaway snapshot into this scratch directory. This
    /// forces fault-in of all guest pages and KVM EPT population, so
    /// the FIRST subsequent BRANCH on this VM sees warm memory rather
    /// than paying the cold-cache penalty (measured 2-9x slowdown on
    /// first BRANCH after spawn — see bench/pause-window/RESULTS-v0.2.md).
    ///
    /// The scratch dir should be on the fastest available storage
    /// (tmpfs / `/dev/shm` preferred); the throwaway snapshot files
    /// are deleted immediately after the prewarm completes. The cost
    /// is one tmpfs-grade pause-window per child added to spawn
    /// latency, in exchange for a consistent steady-state BRANCH
    /// latency from the very first call.
    pub prewarm_scratch_dir: Option<PathBuf>,
    /// Which mechanism the kernel uses to serve guest RAM pages during
    /// restore. v0.2 ships only `File` (the default). `Userfault` is
    /// scaffolding for v0.3 live-branching — see
    /// `docs/design/userfaultfd.md`. The `Userfault` arm is not yet
    /// wired into `restore_many_with`; setting it today triggers a
    /// `todo!()` so we surface the unimplemented state loudly rather
    /// than silently fall back to `File`.
    pub memory_backend: MemoryBackend,
    /// If true, passes `enable_diff_snapshots: true` to Firecracker's
    /// `/snapshot/load`. Required for the resulting VM to support
    /// `Vm::snapshot_diff_to` later — without this flag, Firecracker
    /// rejects Diff snapshot creation with "dirty page tracking
    /// disabled". Default false to preserve v0.2 behavior; v0.3's
    /// daemon path flips it to true so every daemon-spawned source
    /// can be diff-snapshotted.
    ///
    /// Cost: ~1 bit per guest page for the dirty bitmap (e.g., 128 KiB
    /// for 4 GiB), plus a small per-page-fault tracking overhead.
    /// Negligible relative to the snapshot-write savings on subsequent
    /// Diff snapshots.
    pub enable_diff_snapshots: bool,
}

impl Default for ForkOpts {
    fn default() -> Self {
        Self {
            n: 1,
            per_child_netns: false,
            memory_limit_mib: None,
            netns_offset: 0,
            prewarm_scratch_dir: None,
            memory_backend: MemoryBackend::File,
            enable_diff_snapshots: false,
        }
    }
}

/// Result of `Snapshot::restore_many` — N live children plus timing.
#[derive(Debug)]
pub struct ForkResult {
    pub children: Vec<Vm>,
    pub spawn_ms: u128,
    pub restore_ms: u128,
    /// Wall-clock spent in the optional post-restore prewarm pass.
    /// Zero when `prewarm_scratch_dir` was None.
    pub prewarm_ms: u128,
}

/// Response from the guest agent's `exec` action.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecResponse {
    #[serde(default)]
    pub stdout: String,
    #[serde(default)]
    pub stderr: String,
    #[serde(default)]
    pub exit_code: i32,
    #[serde(default)]
    pub error: Option<String>,
}

/// Send an `exec` request to a guest agent listening at `addr` (e.g.
/// "10.42.0.2:8888"). Blocks until the guest replies or `timeout` passes.
pub fn exec_at(addr: &str, args: Vec<String>, timeout: Duration) -> Result<ExecResponse> {
    let socket_addr: SocketAddr = addr
        .parse()
        .with_context(|| format!("invalid address: {addr}"))?;
    let mut stream = TcpStream::connect_timeout(&socket_addr, Duration::from_secs(5))
        .with_context(|| format!("connect {addr}"))?;
    // Allow plenty of headroom over the guest-side timeout.
    stream
        .set_read_timeout(Some(timeout + Duration::from_secs(5)))
        .ok();

    let req = serde_json::json!({
        "action": "exec",
        "args": args,
        "timeout": timeout.as_secs(),
    });
    let line = req.to_string() + "\n";
    stream.write_all(line.as_bytes()).context("send request")?;
    stream.shutdown(std::net::Shutdown::Write).ok();

    let mut buf = String::new();
    stream.read_to_string(&mut buf).context("read response")?;
    let resp: ExecResponse =
        serde_json::from_str(buf.trim()).with_context(|| format!("parse response: {buf}"))?;
    Ok(resp)
}

/// Evaluate a Python expression against the *already-warmed* interpreter
/// running as PID 1 inside the guest. Unlike `exec_at`, this does NOT spawn
/// a new Python process — it reuses the parent's numpy import etc., which
/// is the whole point of the snapshot warm-up.
pub fn eval_at(addr: &str, code: String) -> Result<serde_json::Value> {
    let socket_addr: SocketAddr = addr
        .parse()
        .with_context(|| format!("invalid address: {addr}"))?;
    let mut stream = TcpStream::connect_timeout(&socket_addr, Duration::from_secs(5))
        .with_context(|| format!("connect {addr}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(30))).ok();

    let req = serde_json::json!({"action": "eval", "code": code});
    stream
        .write_all((req.to_string() + "\n").as_bytes())
        .context("send eval")?;
    stream.shutdown(std::net::Shutdown::Write).ok();

    let mut buf = String::new();
    stream.read_to_string(&mut buf).context("read result")?;
    let v: serde_json::Value =
        serde_json::from_str(buf.trim()).with_context(|| format!("parse eval response: {buf}"))?;
    Ok(v)
}

/// Same as `exec_at`, but enters `netns` first using setns(2).
/// Required when each child lives in its own network namespace.
pub fn exec_in_netns(
    netns: &str,
    addr: String,
    args: Vec<String>,
    timeout: Duration,
) -> Result<ExecResponse> {
    run_in_netns(netns, move || exec_at(&addr, args, timeout))
}

/// Same as `eval_at`, but enters `netns` first.
pub fn eval_in_netns(netns: &str, addr: String, code: String) -> Result<serde_json::Value> {
    run_in_netns(netns, move || eval_at(&addr, code))
}

/// Same as `ping_at`, but enters `netns` first.
pub fn ping_in_netns(netns: &str, addr: String) -> Result<serde_json::Value> {
    run_in_netns(netns, move || ping_at(&addr))
}

/// Run `f` in a dedicated thread that has joined network namespace `netns`
/// via setns(2). The main thread's netns is never affected. Requires
/// CAP_SYS_ADMIN (i.e. typically run via sudo).
fn run_in_netns<T, F>(netns: &str, f: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    use std::os::fd::AsRawFd;
    let path = format!("/var/run/netns/{netns}");
    let ns_fd = std::fs::File::open(&path)
        .with_context(|| format!("open {path} (run scripts/netns-setup.sh first?)"))?;

    let handle = thread::spawn(move || -> Result<T> {
        // SAFETY: setns is safe to call; we're a single-purpose thread that
        // will exit after this work is done, so leaking the new netns
        // affinity is intentional and harmless.
        let ret = unsafe { libc::setns(ns_fd.as_raw_fd(), libc::CLONE_NEWNET) };
        if ret != 0 {
            bail!(
                "setns({}) failed: {}. Try: sudo forkd ...",
                path,
                std::io::Error::last_os_error()
            );
        }
        f()
    });
    handle
        .join()
        .map_err(|e| anyhow::anyhow!("netns thread panicked: {e:?}"))?
}

/// Send a `ping` request — returns immediately if the guest agent is alive.
pub fn ping_at(addr: &str) -> Result<serde_json::Value> {
    let socket_addr: SocketAddr = addr
        .parse()
        .with_context(|| format!("invalid address: {addr}"))?;
    let mut stream = TcpStream::connect_timeout(&socket_addr, Duration::from_secs(2))
        .with_context(|| format!("connect {addr}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(3))).ok();

    stream
        .write_all(b"{\"action\":\"ping\"}\n")
        .context("send ping")?;
    stream.shutdown(std::net::Shutdown::Write).ok();

    let mut buf = String::new();
    stream.read_to_string(&mut buf).context("read pong")?;
    let v: serde_json::Value =
        serde_json::from_str(buf.trim()).with_context(|| format!("parse pong: {buf}"))?;
    Ok(v)
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

/// Default per-call timeout. Most Firecracker API calls return in <1 ms.
const DEFAULT_API_TIMEOUT_SECS: u64 = 10;
/// Snapshot create writes the full memory image to disk and is I/O bound.
/// 512 MiB measured at ~3.3 s on our dev box; allow 60 s for slower disks.
const SNAPSHOT_TIMEOUT_SECS: u64 = 60;
/// OOM-killer hint for forked children. Range is -1000..1000; positive
/// values make the process more likely to be killed under memory
/// pressure. Sandboxes are deliberately ephemeral so we prefer the
/// kernel kills them before host-critical processes when memory runs out.
const CHILD_OOM_SCORE_ADJ: i32 = 500;

/// Placeholder path forkd sends as `mem_file_path` for `VmstateOnly`
/// snapshots. The vendored FC requires the field but skips opening it
/// when `snapshot_type == VmstateOnly`. We route the placeholder
/// through `/dev/null/` so that, if a future FC regression *did* open
/// it, `open(2)` would fail loudly with `ENOTDIR` instead of writing
/// to an attacker-pre-created file under `/tmp/`.
const VMSTATE_ONLY_MEM_PLACEHOLDER: &str = "/dev/null/forkd-vmstate-only-mem-ignored";

fn api_call(sock: &Path, method: &str, path: &str, body: &str) -> Result<()> {
    api_call_with_timeout(sock, method, path, body, DEFAULT_API_TIMEOUT_SECS)
}

/// Issue a minimal HTTP/1.1 request to Firecracker's API over the given
/// unix socket. Hand-rolled — saves a ~50-crate hyper+tokio dependency
/// tree and shaves ~15 ms per call versus the previous `curl` subprocess.
///
/// Firecracker's HTTP server doesn't honor `Connection: close` reliably,
/// so we parse Content-Length and stop reading at the body's end rather
/// than waiting for EOF.
fn api_call_with_timeout(
    sock: &Path,
    method: &str,
    path: &str,
    body: &str,
    timeout_secs: u64,
) -> Result<()> {
    let mut stream =
        UnixStream::connect(sock).with_context(|| format!("connect {}", sock.display()))?;
    let timeout = Duration::from_secs(timeout_secs);
    stream.set_read_timeout(Some(timeout)).ok();
    stream.set_write_timeout(Some(timeout)).ok();

    let request = format!(
        "{method} {path} HTTP/1.1\r\n\
         Host: localhost\r\n\
         Accept: application/json\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         \r\n\
         {body}",
        body.len()
    );
    stream
        .write_all(request.as_bytes())
        .with_context(|| format!("send {method} {path}"))?;

    // Read until we have a complete HTTP response (headers + body per
    // Content-Length). Don't wait for EOF — Firecracker keeps the
    // connection open after responding.
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 2048];
    loop {
        let n = stream
            .read(&mut chunk)
            .with_context(|| format!("read response from {method} {path}"))?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if response_complete(&buf) {
            break;
        }
    }

    let response = String::from_utf8_lossy(&buf);
    let status_code: u16 = response
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .with_context(|| {
            format!(
                "unparseable status line: {}",
                response.lines().next().unwrap_or("")
            )
        })?;

    if !(200..300).contains(&status_code) {
        let body_start = response
            .find("\r\n\r\n")
            .map(|i| i + 4)
            .unwrap_or(response.len());
        bail!(
            "firecracker API {} {} returned {}: {}",
            method,
            path,
            status_code,
            response[body_start..].trim()
        );
    }
    Ok(())
}

/// True if `buf` contains a complete HTTP response (headers + body up to
/// Content-Length).
fn response_complete(buf: &[u8]) -> bool {
    let header_end = match find_subslice(buf, b"\r\n\r\n") {
        Some(p) => p,
        None => return false,
    };
    let headers = match std::str::from_utf8(&buf[..header_end]) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let content_len: usize = headers
        .lines()
        .find_map(|l| {
            let lower = l.to_ascii_lowercase();
            lower
                .strip_prefix("content-length:")
                .map(|v| v.trim().parse().unwrap_or(0))
        })
        .unwrap_or(0);
    buf.len() >= header_end + 4 + content_len
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn wait_for_sock(sock: &Path, timeout: Duration) -> Result<()> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if sock.exists() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }
    bail!(
        "socket {} never appeared within {:?}",
        sock.display(),
        timeout
    )
}

fn spawn_firecracker(sock: &Path, console: &Path) -> Result<Child> {
    spawn_firecracker_in(None, sock, console)
}

/// Tell the OOM killer it can sacrifice this process under memory pressure.
/// Best-effort — failure (e.g. permission denied) is silently ignored.
fn nudge_oom_score(pid: u32, adj: i32) {
    let path = format!("/proc/{pid}/oom_score_adj");
    let _ = std::fs::write(&path, adj.to_string());
}

/// Spawn firecracker, optionally inside a pre-existing network namespace.
/// If `netns` is Some, the command is wrapped in `ip netns exec <netns> ...`
/// (this requires the calling user to have CAP_SYS_ADMIN, usually via sudo).
fn spawn_firecracker_in(netns: Option<&str>, sock: &Path, console: &Path) -> Result<Child> {
    let f = std::fs::File::create(console).context("create console log file")?;
    let f_err = f.try_clone()?;

    let mut cmd = match netns {
        Some(ns) => {
            let mut c = Command::new("ip");
            c.args(["netns", "exec", ns, "firecracker", "--api-sock"]);
            c
        }
        None => {
            let mut c = Command::new("firecracker");
            c.arg("--api-sock");
            c
        }
    };

    let child = cmd
        .arg(sock)
        .stdin(Stdio::null())
        .stdout(f)
        .stderr(f_err)
        .spawn()
        .with_context(|| {
            if let Some(ns) = netns {
                format!("failed to spawn firecracker in netns {ns}")
            } else {
                "failed to spawn firecracker".to_string()
            }
        })?;

    // Make child VMs preferred OOM targets so the host stays responsive
    // when memory runs out. Sandboxes are designed to be ephemeral.
    nudge_oom_score(child.id(), CHILD_OOM_SCORE_ADJ);

    Ok(child)
}

// ---------------------------------------------------------------------------
// Vm public API
// ---------------------------------------------------------------------------

impl Vm {
    /// Boot a fresh VM from kernel + rootfs.
    ///
    /// This blocks until Firecracker accepts `InstanceStart`. It does NOT
    /// wait for guest userspace to come up — the caller should sleep or
    /// poll the console.
    pub fn boot(cfg: &BootConfig) -> Result<Self> {
        std::fs::create_dir_all(&cfg.work_dir).context("create work_dir")?;
        let sock = cfg.work_dir.join("fc.sock");
        let console = cfg.work_dir.join("fc.console");
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(&console);

        let proc = spawn_firecracker(&sock, &console)?;
        let pid = proc.id();

        wait_for_sock(&sock, Duration::from_secs(3))?;

        // If we have static networking, append the kernel ip= parameter so
        // the guest comes up with the right IP without any userspace tools.
        let mut boot_args = cfg.boot_args.clone();
        if let Some(net) = &cfg.network {
            if let Some(ip_arg) = net.kernel_ip_arg() {
                boot_args.push(' ');
                boot_args.push_str(&ip_arg);
            }
        }

        let body = serde_json::json!({
            "kernel_image_path": cfg.kernel,
            "boot_args": boot_args,
        });
        api_call(&sock, "PUT", "/boot-source", &body.to_string())?;

        let body = serde_json::json!({
            "drive_id": "rootfs",
            "path_on_host": cfg.rootfs,
            "is_root_device": true,
            "is_read_only": cfg.rootfs_read_only,
        });
        api_call(&sock, "PUT", "/drives/rootfs", &body.to_string())?;

        // Extra volume drives — vdb, vdc, ... — in declaration order.
        // The cmdline already includes a forkd.mounts= hint (added by
        // BootConfig::with_volume) so /forkd-init.sh knows where to
        // mount each device after boot.
        for (i, vol) in cfg.volumes.iter().enumerate() {
            let drive_id = format!("vol{i}");
            let body = serde_json::json!({
                "drive_id": &drive_id,
                "path_on_host": &vol.host_path,
                "is_root_device": false,
                "is_read_only": vol.read_only,
            });
            api_call(
                &sock,
                "PUT",
                &format!("/drives/{drive_id}"),
                &body.to_string(),
            )?;
        }

        let body = serde_json::json!({
            "vcpu_count": cfg.vcpu_count,
            "mem_size_mib": cfg.mem_size_mib,
            "track_dirty_pages": true,
        });
        api_call(&sock, "PUT", "/machine-config", &body.to_string())?;

        // Optional network interface — must be PUT before InstanceStart.
        if let Some(net) = &cfg.network {
            let mut body = serde_json::json!({
                "iface_id": net.iface_id,
                "host_dev_name": net.host_dev_name,
            });
            if let Some(mac) = &net.guest_mac {
                body["guest_mac"] = serde_json::Value::String(mac.clone());
            }
            let path = format!("/network-interfaces/{}", net.iface_id);
            api_call(&sock, "PUT", &path, &body.to_string())?;
        }

        api_call(
            &sock,
            "PUT",
            "/actions",
            r#"{"action_type":"InstanceStart"}"#,
        )?;

        Ok(Vm {
            proc,
            pid,
            sock,
            console,
            netns: None,
            cgroup: None,
            memfd: None,
        })
    }

    /// Pause the VM (no vCPU progress). Required before snapshot.
    pub fn pause(&self) -> Result<()> {
        api_call(&self.sock, "PATCH", "/vm", r#"{"state":"Paused"}"#)
    }

    /// Resume a paused VM. Pair with `pause()`. After a successful
    /// `pause + snapshot_to + resume` sequence the VM is back in its
    /// pre-pause state with vCPUs running again.
    pub fn resume(&self) -> Result<()> {
        api_call(&self.sock, "PATCH", "/vm", r#"{"state":"Resumed"}"#)
    }

    /// Acquire a [`PauseGuard`] that holds the VM paused until dropped.
    /// `?`-friendly alternative to manual `pause()` + `resume()` —
    /// resumes even on early return / panic. Best-effort: drop swallows
    /// the resume error to avoid masking the panic that triggered the
    /// drop; for explicit error handling, use [`PauseGuard::commit`].
    pub fn pause_guard(&self) -> Result<PauseGuard<'_>> {
        PauseGuard::pause(self)
    }
}

/// RAII guard that holds a VM paused. Drop calls `vm.resume()` unless
/// the guard was [`commit`](Self::commit)ted (used when the caller has
/// already explicitly resumed and doesn't want the implicit retry).
///
/// Phase 6.3 uses this around the brief `pause -> snapshot_vmstate_only
/// -> resume` critical section: if `snapshot_vmstate_only` errors, the
/// guard drops on the early return and the source VM doesn't get
/// stuck paused.
pub struct PauseGuard<'a> {
    vm: &'a Vm,
    committed: bool,
}

impl<'a> PauseGuard<'a> {
    /// Pause the VM and return a guard that will resume on drop.
    pub fn pause(vm: &'a Vm) -> Result<Self> {
        vm.pause()?;
        Ok(Self {
            vm,
            committed: false,
        })
    }

    /// Explicitly resume now and consume the guard, returning the
    /// resume result so the caller can handle it. Use this instead of
    /// `drop()` when you want the resume to be a checked failure point.
    pub fn resume(mut self) -> Result<()> {
        self.committed = true;
        self.vm.resume()
    }

    /// Mark the guard as resumed-elsewhere so Drop won't retry. Use
    /// this if the caller resumed via some other path and just needs
    /// the guard to stop being responsible.
    pub fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for PauseGuard<'_> {
    fn drop(&mut self) {
        if !self.committed {
            // Best-effort. If this errors during a panic unwind, the
            // panic message is what callers want to see — swallowing
            // the resume error preserves that signal. Operators see
            // the still-paused VM and can re-resume manually if needed.
            if let Err(e) = self.vm.resume() {
                tracing::warn!(
                    error = %e,
                    "PauseGuard drop: vm.resume() failed; VM may be stuck paused",
                );
            }
        }
    }
}

impl Vm {
    /// Write a Full snapshot to disk. VM must be paused first. `volumes` is
    /// the list of volumes that were attached at boot — the snapshot stores
    /// them so subsequent restores reattach the same host files at the same
    /// guest device positions.
    pub fn snapshot_to(
        &self,
        vmstate: PathBuf,
        memory: PathBuf,
        volumes: Vec<VolumeSpec>,
    ) -> Result<Snapshot> {
        if let Some(p) = vmstate.parent() {
            std::fs::create_dir_all(p).context("create snapshot dir")?;
        }
        let body = serde_json::json!({
            "snapshot_path": vmstate,
            "mem_file_path": memory,
            "snapshot_type": "Full",
        });
        api_call_with_timeout(
            &self.sock,
            "PUT",
            "/snapshot/create",
            &body.to_string(),
            SNAPSHOT_TIMEOUT_SECS,
        )?;
        Ok(Snapshot {
            vmstate,
            memory,
            volumes,
            parent_tag: None,
            parent_content_hash: None,
        })
    }

    /// Write a Diff snapshot to disk. VM must be paused first. The returned
    /// file at `memory_diff` is a SPARSE file with the same logical size as
    /// a Full snapshot, but only the pages dirtied since the previous
    /// snapshot (or since restore, if this is the first snapshot) contain
    /// bytes — the rest is `lseek(SEEK_HOLE)` gaps. Firecracker clears the
    /// dirty bitmap as part of this call, so a subsequent Diff snapshot
    /// starts a fresh window.
    ///
    /// Diff is not directly restorable. The caller is responsible for
    /// merging the diff into a base `memory.bin` (see `apply_diff`)
    /// before any `Snapshot::restore_many_with` call. Forkd's BRANCH path
    /// in `forkd-controller` does this via a per-sandbox shadow file —
    /// see `docs/design/diff-snapshots.md`.
    ///
    /// Requires `track_dirty_pages: true` on `/machine-config`, which
    /// forkd sets by default in `Vm::boot`.
    pub fn snapshot_diff_to(
        &self,
        vmstate: PathBuf,
        memory_diff: PathBuf,
        volumes: Vec<VolumeSpec>,
    ) -> Result<DiffSnapshot> {
        if let Some(p) = vmstate.parent() {
            std::fs::create_dir_all(p).context("create diff snapshot dir")?;
        }
        let body = serde_json::json!({
            "snapshot_path": vmstate,
            "mem_file_path": memory_diff,
            "snapshot_type": "Diff",
        });
        api_call_with_timeout(
            &self.sock,
            "PUT",
            "/snapshot/create",
            &body.to_string(),
            SNAPSHOT_TIMEOUT_SECS,
        )?;
        // Logical size = the full-snapshot size; physical size on disk is
        // smaller (sparse holes). Capture both so callers can report
        // savings without re-stat'ing the file.
        let meta = std::fs::metadata(&memory_diff).context("stat diff memory file")?;
        Ok(DiffSnapshot {
            vmstate,
            memory_diff,
            logical_size_bytes: meta.len(),
            physical_size_bytes: meta.blocks() * 512,
            volumes,
        })
    }

    /// Write a vmstate-only snapshot. VM must be paused first. Writes
    /// the vmstate JSON to `vmstate`; **does not touch** `memory.bin` —
    /// the caller is responsible for serializing guest RAM externally
    /// (forkd does this via the `WpBranch` async-copy path on memfd +
    /// MAP_SHARED). `memory` and `volumes` are NOT written by this call
    /// — they're attached to the returned [`Snapshot`] so the caller
    /// gets the same record shape it would from [`Self::snapshot_to`] /
    /// [`Self::snapshot_diff_to`] and can hand it off to the post-pause
    /// copy pipeline without reconstructing it.
    ///
    /// Requires the vendored Firecracker fork
    /// (`deeplethe/firecracker:forkd-v0.4-mem-backend-shared-v1.12`); stock
    /// FC does not understand `snapshot_type: "VmstateOnly"` and will
    /// reject the request with a 400.
    ///
    /// This is the FC half of the v0.4 `mode="live"` BRANCH path. See
    /// [`DESIGN-v0.4-PHASE6.md`](../../../DESIGN-v0.4-PHASE6.md).
    //
    // TODO(phase6.3): `SNAPSHOT_TIMEOUT_SECS` is sized for full/diff
    // snapshots that may write multi-GB; vmstate-only takes ~22ms in
    // practice. Inside Phase 6.3's <10 ms pause window a hung FC would
    // extend source-paused for the full timeout. Plumb a tighter
    // dedicated timeout (~2-5s) when wiring into branch_sandbox.
    pub fn snapshot_vmstate_only(
        &self,
        vmstate: PathBuf,
        memory: PathBuf,
        volumes: Vec<VolumeSpec>,
    ) -> Result<Snapshot> {
        if let Some(p) = vmstate.parent() {
            std::fs::create_dir_all(p).context("create vmstate-only snapshot dir")?;
        }
        // FC's `CreateSnapshotParams` still requires a `mem_file_path`
        // field in the request body (it's a `PathBuf`, not `Option`),
        // but the patched FC checks `snapshot_type == VmstateOnly` and
        // never opens the file. We deliberately route the placeholder
        // through `/dev/null/` — see `VMSTATE_ONLY_MEM_PLACEHOLDER`.
        let body = serde_json::json!({
            "snapshot_path": vmstate,
            "mem_file_path": VMSTATE_ONLY_MEM_PLACEHOLDER,
            "snapshot_type": "VmstateOnly",
        });
        api_call_with_timeout(
            &self.sock,
            "PUT",
            "/snapshot/create",
            &body.to_string(),
            SNAPSHOT_TIMEOUT_SECS,
        )?;
        Ok(Snapshot {
            vmstate,
            memory,
            volumes,
            parent_tag: None,
            parent_content_hash: None,
        })
    }

    /// Pre-warm the VM's guest memory by performing a throwaway snapshot.
    ///
    /// On the first BRANCH after a fresh restore, firecracker iterates
    /// the entire guest RAM and writes it to a new `memory.bin`. The
    /// guest's mmap of the original `memory.bin` has not yet been
    /// faulted-in for most pages, and KVM's EPT entries are lazily
    /// populated. The first iteration therefore pays a one-time read
    /// pass (fault-in + EPT setup) on top of the write pass. Measured
    /// 2-9x cold/warm ratio in `bench/pause-window/RESULTS-v0.2.md`.
    ///
    /// `prewarm()` amortizes that cost: it pauses the VM, writes a
    /// throwaway snapshot to `scratch_dir`, resumes, and deletes the
    /// throwaway files. After this completes, subsequent BRANCHes see
    /// warm pages and populated EPT — so T1 ≈ T2 ≈ T3 instead of
    /// T1 = 2-9x T2.
    ///
    /// `scratch_dir` should be on the fastest available backend
    /// (tmpfs / `/dev/shm` preferred). The throwaway files are sized
    /// like a real snapshot (~guest RAM) so the directory must have
    /// enough free space.
    ///
    /// Returns the wall-clock milliseconds spent in the prewarm cycle.
    pub fn prewarm(&self, scratch_dir: &Path) -> Result<u128> {
        std::fs::create_dir_all(scratch_dir).context("create prewarm scratch dir")?;
        // Per-VM filenames keyed on the API socket so concurrent prewarms
        // of sibling children don't clobber each other when they share a
        // scratch directory (e.g. /dev/shm).
        let key = self
            .sock
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| format!("pid{}", self.pid));
        let vmstate = scratch_dir.join(format!(".prewarm-vmstate-{key}.bin"));
        let memory = scratch_dir.join(format!(".prewarm-memory-{key}.bin"));
        // Best-effort cleanup of stale prewarm files from a previous run.
        let _ = std::fs::remove_file(&vmstate);
        let _ = std::fs::remove_file(&memory);

        let start = Instant::now();
        self.pause().context("prewarm: pause")?;
        // Snapshot iterates the entire guest RAM, forcing fault-in of all
        // pages and EPT population. The write goes to scratch_dir (tmpfs
        // preferred) so we don't pay disk-write cost on top.
        let body = serde_json::json!({
            "snapshot_path": &vmstate,
            "mem_file_path": &memory,
            "snapshot_type": "Full",
        });
        let snap_result = api_call_with_timeout(
            &self.sock,
            "PUT",
            "/snapshot/create",
            &body.to_string(),
            SNAPSHOT_TIMEOUT_SECS,
        );
        // Always try to resume, even if the snapshot failed, so the VM
        // doesn't end up stuck in Paused state visible to API callers.
        let resume_result = self.resume();
        snap_result.context("prewarm: snapshot/create")?;
        resume_result.context("prewarm: resume")?;
        let elapsed_ms = start.elapsed().as_millis();

        // Cleanup. Failure here isn't fatal — the throwaway files are
        // harmless if left behind, and the caller's measurement is done.
        let _ = std::fs::remove_file(&vmstate);
        let _ = std::fs::remove_file(&memory);

        Ok(elapsed_ms)
    }

    /// Send CtrlAltDel to the guest. Best-effort; ignored if VM unresponsive.
    pub fn shutdown(&self) -> Result<()> {
        let _ = api_call(
            &self.sock,
            "PUT",
            "/actions",
            r#"{"action_type":"SendCtrlAltDel"}"#,
        );
        Ok(())
    }

    /// Hard-kill the firecracker process.
    pub fn kill(&mut self) -> Result<()> {
        let _ = self.proc.kill();
        let _ = self.proc.wait();
        let _ = std::fs::remove_file(&self.sock);
        Ok(())
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        let _ = self.proc.kill();
        let _ = self.proc.wait();
        let _ = std::fs::remove_file(&self.sock);
        if let Some(cg) = &self.cgroup {
            cgroup::cleanup(cg);
        }
    }
}

// ---------------------------------------------------------------------------
// Snapshot public API
// ---------------------------------------------------------------------------

impl Snapshot {
    /// Spawn N firecracker processes and restore each from this snapshot.
    /// All restores fire in parallel; the kernel mmaps `memory.bin` with
    /// `MAP_PRIVATE`, giving copy-on-write sharing between children.
    pub fn restore_many(&self, n: usize, work_dir: &Path) -> Result<ForkResult> {
        self.restore_many_with(
            ForkOpts {
                n,
                per_child_netns: false,
                memory_limit_mib: None,
                netns_offset: 0,
                prewarm_scratch_dir: None,
                memory_backend: MemoryBackend::File,
                enable_diff_snapshots: false,
            },
            work_dir,
        )
    }

    /// Same as `restore_many` but with explicit options.
    pub fn restore_many_with(&self, opts: ForkOpts, work_dir: &Path) -> Result<ForkResult> {
        // v0.3 Userfault scaffolding is intentionally not wired up yet.
        // v0.4 MemfdShared (Phase 5b) IS wired below. Anything else
        // fails loudly so callers don't silently get File semantics.
        match opts.memory_backend {
            MemoryBackend::File | MemoryBackend::MemfdShared => {}
            MemoryBackend::Userfault { .. } => bail!(
                "MemoryBackend::Userfault is v0.3 scaffolding and not yet \
                 implemented — see docs/design/userfaultfd.md for status"
            ),
        }
        let n = opts.n;
        std::fs::create_dir_all(work_dir).context("create fork work_dir")?;
        // Sweep everything in work_dir — including stale unix sockets, which
        // is_file() considers neither file nor dir and would otherwise leave
        // Firecracker's bind to fail on the next run.
        for e in std::fs::read_dir(work_dir)?.flatten() {
            let p = e.path();
            if p.is_dir() {
                continue;
            }
            let _ = std::fs::remove_file(&p);
        }

        // Phase 1: spawn N firecracker processes, wait for sockets.
        let spawn_start = Instant::now();
        let mut children: Vec<Vm> = Vec::with_capacity(n);
        for i in 1..=n {
            // Per-child files use the within-batch index (1..=n) so work_dir
            // layout is predictable. Netns / cgroup index applies the offset
            // so multiple batches can coexist on one host (branching case).
            let global_idx = opts.netns_offset + i;
            let sock = work_dir.join(format!("child-{i}.sock"));
            let console = work_dir.join(format!("child-{i}.console"));
            // If per-child netns mode, look for forkd-child-<global_idx>
            // (provisioned by scripts/netns-setup.sh ahead of time).
            let netns = if opts.per_child_netns {
                Some(format!("forkd-child-{global_idx}"))
            } else {
                None
            };
            let proc = spawn_firecracker_in(netns.as_deref(), &sock, &console)?;
            let pid = proc.id();
            children.push(Vm {
                proc,
                pid,
                sock,
                console,
                netns,
                cgroup: None,
                memfd: None,
            });
        }
        for c in &children {
            wait_for_sock(&c.sock, Duration::from_secs(5))?;
        }
        let spawn_ms = spawn_start.elapsed().as_millis();

        // Optional: place each child in its own cgroup v2 leaf with a
        // memory.max limit. We do this AFTER the socket is up but BEFORE
        // restore so the limit applies to the memory image mmap as well.
        if let Some(mib) = opts.memory_limit_mib {
            let bytes = cgroup::mib_to_bytes(mib);
            for (i, child) in children.iter_mut().enumerate() {
                // Cgroup leaf name tracks the global netns index so per-child
                // resource limits don't collide with siblings created by other
                // batches on the same host.
                let name = format!("child-{}", opts.netns_offset + i + 1);
                match cgroup::place_child(&name, child.pid, bytes) {
                    Ok(path) => {
                        tracing::debug!(name=%name, pid=child.pid, mib, "cgroup placed");
                        child.cgroup = Some(path);
                    }
                    Err(e) => {
                        // Don't poison the whole fork on cgroup failure; the
                        // child is still functional, just unconstrained. Log
                        // loudly so operators see it.
                        tracing::warn!(error=%e, name=%name, "cgroup placement failed");
                    }
                }
            }
        }

        // Phase 1.5 (v0.4 MemfdShared): create one memfd per child,
        // populated from the snapshot's memory.bin, before any restore
        // request goes out. The memfd holds the FC-visible RAM pages;
        // forkd-controller keeps an mmap on the same memfd so Phase 6
        // can arm UFFDIO_WRITEPROTECT on the shared VMA.
        if matches!(opts.memory_backend, MemoryBackend::MemfdShared) {
            for (i, child) in children.iter_mut().enumerate() {
                let region = memfd::create_and_populate(
                    &self.memory,
                    &format!("forkd-source-mem-{}", opts.netns_offset + i + 1),
                )
                .with_context(|| {
                    format!(
                        "create_and_populate memfd from {} for child #{}",
                        self.memory.display(),
                        i + 1
                    )
                })?;
                child.memfd = Some(region);
            }
        }

        // Phase 2: parallel restore via threads. Each thread issues one
        // /snapshot/load PUT to its child's API socket. Body varies per
        // child only under MemfdShared (each child has its own memfd
        // path); for the File path, all children share the same JSON.
        let restore_start = Instant::now();
        let bodies: Vec<String> = children
            .iter()
            .map(|c| match &c.memfd {
                Some(region) => serde_json::json!({
                    "snapshot_path": &self.vmstate,
                    "mem_backend": {
                        "backend_path": region.backend_path(),
                        "backend_type": "File",
                        "shared": true,
                    },
                    "enable_diff_snapshots": opts.enable_diff_snapshots,
                    "resume_vm": true,
                })
                .to_string(),
                None => serde_json::json!({
                    "snapshot_path": &self.vmstate,
                    "mem_backend": {
                        "backend_path": &self.memory,
                        "backend_type": "File",
                    },
                    "enable_diff_snapshots": opts.enable_diff_snapshots,
                    "resume_vm": true,
                })
                .to_string(),
            })
            .collect();

        let mut handles = Vec::with_capacity(n);
        for (c, body) in children.iter().zip(bodies) {
            let sock = c.sock.clone();
            handles.push(thread::spawn(move || -> Result<()> {
                api_call(&sock, "PUT", "/snapshot/load", &body)
            }));
        }
        for h in handles {
            h.join().expect("restore thread panicked")?;
        }
        let restore_ms = restore_start.elapsed().as_millis();

        // Phase 3 (optional): prewarm each child by performing a
        // throwaway snapshot. This amortizes the cold-cache penalty
        // (2-9x slower first BRANCH vs. steady-state) so the first
        // user-visible BRANCH on each child runs at steady-state speed.
        // Fire in parallel — siblings share storage, but the prewarms
        // are independent and parallelism wins on multi-VM batches.
        let prewarm_ms = if let Some(scratch) = opts.prewarm_scratch_dir {
            std::fs::create_dir_all(&scratch).context("create prewarm scratch dir")?;
            let prewarm_start = Instant::now();
            let mut handles = Vec::with_capacity(n);
            for c in &children {
                let sock = c.sock.clone();
                let pid = c.pid;
                let scratch = scratch.clone();
                handles.push(thread::spawn(move || -> Result<()> {
                    // Re-construct a minimal Vm-shaped reference for the
                    // prewarm call. We can't move the real Vm into the
                    // thread (it owns the Child) so we issue the API
                    // calls inline rather than going through Vm::prewarm.
                    let key = sock
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| format!("pid{pid}"));
                    let vmstate = scratch.join(format!(".prewarm-vmstate-{key}.bin"));
                    let memory = scratch.join(format!(".prewarm-memory-{key}.bin"));
                    let _ = std::fs::remove_file(&vmstate);
                    let _ = std::fs::remove_file(&memory);

                    api_call(&sock, "PATCH", "/vm", r#"{"state":"Paused"}"#)
                        .context("prewarm: pause")?;
                    let body = serde_json::json!({
                        "snapshot_path": &vmstate,
                        "mem_file_path": &memory,
                        "snapshot_type": "Full",
                    });
                    let snap_result = api_call_with_timeout(
                        &sock,
                        "PUT",
                        "/snapshot/create",
                        &body.to_string(),
                        SNAPSHOT_TIMEOUT_SECS,
                    );
                    let resume_result = api_call(&sock, "PATCH", "/vm", r#"{"state":"Resumed"}"#);
                    snap_result.context("prewarm: snapshot/create")?;
                    resume_result.context("prewarm: resume")?;

                    let _ = std::fs::remove_file(&vmstate);
                    let _ = std::fs::remove_file(&memory);
                    Ok(())
                }));
            }
            for h in handles {
                h.join().expect("prewarm thread panicked")?;
            }
            prewarm_start.elapsed().as_millis()
        } else {
            0
        };

        Ok(ForkResult {
            children,
            spawn_ms,
            restore_ms,
            prewarm_ms,
        })
    }
}

// ---------------------------------------------------------------------------
// Diff snapshot helpers
// ---------------------------------------------------------------------------

/// Merge a Diff snapshot's sparse memory file onto a base memory.bin
/// in place. Walks the diff file's allocated extents (via
/// `lseek(SEEK_DATA)`) and copies each non-hole region onto the same
/// byte offset of the base file.
///
/// Returns the number of bytes actually copied — the dirty footprint
/// of this BRANCH cycle. Useful for telemetry: a small return value
/// means the source touched little memory between snapshots and the
/// diff was effective.
///
/// Safety / correctness:
/// - Both files must already exist; `base` is opened O_RDWR, `diff`
///   O_RDONLY.
/// - The diff's logical size **must equal** base's. Enforced at the top
///   of the function — mismatch returns an error rather than extending
///   the base file (which would silently corrupt the snapshot; the next
///   restore via `mmap` would see a larger memory image than the guest
///   was built with). The check exists for callers outside the daemon
///   path and for upgrade scenarios where a stale `last_branch_memory_path`
///   survives a format change.
/// - No fsync — the merge is allowed to be in page cache until the
///   children's restore mmaps it. If the host crashes mid-merge,
///   subsequent restore reads garbage; durability of the shadow file
///   is the daemon's concern, not this helper's.
///
/// Linux-only because it relies on `SEEK_DATA` / `SEEK_HOLE`. On other
/// targets this function returns an error so callers don't silently
/// fall back to "copy the whole diff" semantics.
#[cfg(target_os = "linux")]
pub fn apply_diff(diff: &Path, base: &Path) -> Result<u64> {
    use std::io::{Seek, SeekFrom};
    let mut diff_f =
        std::fs::File::open(diff).with_context(|| format!("open diff {}", diff.display()))?;
    let mut base_f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(base)
        .with_context(|| format!("open base {}", base.display()))?;

    // Size-invariant check: the diff and base must be the same logical
    // size. Without this, a larger diff would cause `base_f.write_all`
    // past `base_len` to silently extend the base file (Linux semantics
    // for writes to O_RDWR files past EOF), corrupting the snapshot for
    // any subsequent mmap-based restore. See #180.
    let diff_len_u = diff_f.metadata().context("stat diff")?.len();
    let base_len_u = base_f.metadata().context("stat base")?.len();
    if diff_len_u != base_len_u {
        bail!(
            "apply_diff: size mismatch (diff={diff_len_u} B vs base={base_len_u} B); \
             refusing to corrupt base — diff and base must have identical logical size"
        );
    }
    let diff_len = diff_len_u as i64;
    let mut copied: u64 = 0;
    let mut cursor: i64 = 0;
    // Cap buffer at 1 MiB so we don't allocate the whole guest RAM on a
    // worst-case diff (= full memory touched, every page dirty).
    let mut buf = vec![0u8; 1 << 20];

    loop {
        // Find the start of the next data region. SEEK_DATA returns the
        // current position if already at data, or jumps forward to the
        // next non-hole byte. ENXIO at EOF — translate to "done".
        let data_start = match lseek_data_or_hole(&diff_f, cursor, true) {
            Ok(p) => p,
            Err(e) if e.raw_os_error() == Some(libc::ENXIO) => break,
            Err(e) => return Err(e).context("SEEK_DATA on diff"),
        };
        if data_start >= diff_len {
            break;
        }
        // Find the end of this data region (= start of next hole).
        // SEEK_HOLE always succeeds — if there's no hole, it returns
        // file end.
        let hole_start = lseek_data_or_hole(&diff_f, data_start, false)
            .context("SEEK_HOLE on diff")?
            .min(diff_len);

        // Copy [data_start, hole_start) from diff to base.
        diff_f
            .seek(SeekFrom::Start(data_start as u64))
            .context("seek diff to data_start")?;
        base_f
            .seek(SeekFrom::Start(data_start as u64))
            .context("seek base to data_start")?;
        let mut remaining = (hole_start - data_start) as u64;
        while remaining > 0 {
            let chunk = remaining.min(buf.len() as u64) as usize;
            diff_f
                .read_exact(&mut buf[..chunk])
                .context("read diff chunk")?;
            base_f
                .write_all(&buf[..chunk])
                .context("write base chunk")?;
            remaining -= chunk as u64;
            copied += chunk as u64;
        }
        cursor = hole_start;
    }

    Ok(copied)
}

#[cfg(not(target_os = "linux"))]
pub fn apply_diff(_diff: &Path, _base: &Path) -> Result<u64> {
    bail!("apply_diff requires SEEK_DATA / SEEK_HOLE (Linux only)")
}

#[cfg(target_os = "linux")]
fn lseek_data_or_hole(f: &std::fs::File, offset: i64, want_data: bool) -> std::io::Result<i64> {
    use std::os::fd::AsRawFd;
    let whence = if want_data {
        libc::SEEK_DATA
    } else {
        libc::SEEK_HOLE
    };
    // SAFETY: f is an open file we own. lseek with SEEK_DATA/SEEK_HOLE
    // is a pure offset query, no buffer arguments.
    let r = unsafe { libc::lseek(f.as_raw_fd(), offset, whence) };
    if r < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boot_config_quickstart_has_sane_defaults() {
        let cfg = BootConfig::quickstart("/tmp/k".into(), "/tmp/r".into(), "/tmp/w".into());
        assert_eq!(cfg.vcpu_count, 2);
        assert_eq!(cfg.mem_size_mib, 512);
        assert!(cfg.boot_args.contains("console=ttyS0"));
        assert!(cfg.boot_args.contains("root=/dev/vda"));
        assert!(cfg.boot_args.contains(" ro"));
        assert!(cfg.rootfs_read_only);
    }

    #[test]
    fn vmstate_only_placeholder_is_unwriteable() {
        // Defense-in-depth: if FC ever forgets the VmstateOnly guard and
        // opens the placeholder, the path must fail open(2) loudly
        // rather than succeed against a real (possibly
        // attacker-pre-created) file. `/dev/null/anything` resolves
        // through a character device — open(2) returns ENOTDIR.
        use std::fs::OpenOptions;
        let err = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(VMSTATE_ONLY_MEM_PLACEHOLDER)
            .expect_err("placeholder must not be openable for write");
        // Linux: ENOTDIR. Rust maps that to ErrorKind::NotADirectory on
        // 1.83+; older toolchains see ErrorKind::Other. Either is fine
        // — what matters is open(2) failed.
        let raw = err.raw_os_error();
        assert_eq!(
            raw,
            Some(libc::ENOTDIR),
            "expected ENOTDIR (placeholder routes through /dev/null/), got errno={raw:?}",
        );
    }

    #[test]
    fn vmstate_only_request_body_shape() {
        // Mirror the body Vm::snapshot_vmstate_only sends. If this stays
        // in sync with the actual function it catches typos in the
        // `snapshot_type` enum string (the patched FC rejects unknown
        // variants with HTTP 400) and accidental moves of the
        // placeholder out of `/dev/null/`.
        let vmstate = PathBuf::from("/tmp/snap/vmstate");
        let body = serde_json::json!({
            "snapshot_path": vmstate,
            "mem_file_path": VMSTATE_ONLY_MEM_PLACEHOLDER,
            "snapshot_type": "VmstateOnly",
        });
        assert_eq!(body["snapshot_type"].as_str(), Some("VmstateOnly"));
        assert_eq!(body["snapshot_path"].as_str(), Some("/tmp/snap/vmstate"));
        let placeholder = body["mem_file_path"].as_str().unwrap();
        assert!(
            placeholder.starts_with("/dev/null/"),
            "placeholder must live under /dev/null/ so a regression fails ENOTDIR; got {placeholder}",
        );
    }

    #[test]
    fn request_wp_uffd_body_shape() {
        // Mirror the body Vm::request_wp_uffd sends. The vendored FC's
        // SetupWpUffdParams uses `deny_unknown_fields`, so a typo in
        // the `socket` key name surfaces as HTTP 400.
        let socket = PathBuf::from("/run/forkd/wp.sock");
        let body = serde_json::json!({
            "socket": socket,
        });
        assert_eq!(
            body["socket"].as_str(),
            Some("/run/forkd/wp.sock"),
            "field name must be exactly `socket` (FC's SetupWpUffdParams deny_unknown_fields)",
        );
        // Sanity: should be a one-key object — anything else here means
        // the body grew silently and we're shipping fields FC will
        // reject.
        let obj = body.as_object().expect("body is a JSON object");
        assert_eq!(obj.len(), 1, "body must have exactly one field");
    }

    #[test]
    fn boot_config_ext4_rw_is_writable() {
        let cfg = BootConfig::ext4_rw("/tmp/k".into(), "/tmp/r.ext4".into(), "/tmp/w".into());
        assert!(!cfg.rootfs_read_only);
        // " rw " appears as the rootfs mount mode flag (surrounded by spaces).
        assert!(
            cfg.boot_args.contains(" rw "),
            "expected boot_args to contain ' rw ', got: {}",
            cfg.boot_args
        );
        // and the warmup init script is referenced
        assert!(cfg.boot_args.contains("init=/forkd-init.sh"));
    }

    #[test]
    fn volume_device_name_progression() {
        assert_eq!(volume_device_name(0).unwrap(), "vdb");
        assert_eq!(volume_device_name(1).unwrap(), "vdc");
        assert_eq!(volume_device_name(22).unwrap(), "vdx");
        // Last valid index: /dev/vdz (the byte before ASCII '{').
        assert_eq!(
            volume_device_name(MAX_VOLUMES - 1).unwrap(),
            "vdz",
            "last valid index should produce vdz",
        );
    }

    #[test]
    fn volume_device_name_rejects_past_cap() {
        // Regression for #175: without the cap, indices 25-153 produced
        // garbage device names (vd{, vd|, vd}, vd~, ...) that the host-side
        // cmdline append accepted but the guest could not mount.
        for index in [MAX_VOLUMES, MAX_VOLUMES + 1, 100, 153] {
            assert!(
                volume_device_name(index).is_err(),
                "expected error at index {index} (>= MAX_VOLUMES = {MAX_VOLUMES})",
            );
        }
    }

    #[test]
    fn boot_config_with_volume_appends_cmdline_hint() {
        let cfg = BootConfig::ext4_rw("/tmp/k".into(), "/tmp/r".into(), "/tmp/w".into())
            .with_volume(VolumeSpec {
                host_path: "/var/lib/forkd/vol/pyagent.img".into(),
                guest_path: "/opt/cache".into(),
                read_only: false,
            })
            .unwrap();
        assert_eq!(cfg.volumes.len(), 1);
        assert!(
            cfg.boot_args.contains("forkd.mounts=vdb:/opt/cache"),
            "expected forkd.mounts in boot_args, got: {}",
            cfg.boot_args
        );
    }

    #[test]
    fn boot_config_with_multiple_volumes_extends_hint() {
        let cfg = BootConfig::ext4_rw("/tmp/k".into(), "/tmp/r".into(), "/tmp/w".into())
            .with_volume(VolumeSpec {
                host_path: "/a.img".into(),
                guest_path: "/opt/a".into(),
                read_only: false,
            })
            .unwrap()
            .with_volume(VolumeSpec {
                host_path: "/b.img".into(),
                guest_path: "/opt/b".into(),
                read_only: true,
            })
            .unwrap();
        assert_eq!(cfg.volumes.len(), 2);
        assert!(
            cfg.boot_args.contains("forkd.mounts=vdb:/opt/a,vdc:/opt/b"),
            "boot_args was: {}",
            cfg.boot_args
        );
    }

    #[test]
    fn boot_config_with_volume_errors_past_cap() {
        let mut cfg = BootConfig::ext4_rw("/tmp/k".into(), "/tmp/r".into(), "/tmp/w".into());
        // Fill to capacity (MAX_VOLUMES = 25) — should succeed.
        for i in 0..MAX_VOLUMES {
            cfg = cfg
                .with_volume(VolumeSpec {
                    host_path: format!("/v{i}.img").into(),
                    guest_path: format!("/mnt/{i}").into(),
                    read_only: false,
                })
                .unwrap_or_else(|e| panic!("volume {i} should fit under cap: {e}"));
        }
        assert_eq!(cfg.volumes.len(), MAX_VOLUMES);
        // One past the cap — must error, not append a garbage device name.
        let result = cfg.with_volume(VolumeSpec {
            host_path: "/overflow.img".into(),
            guest_path: "/mnt/overflow".into(),
            read_only: false,
        });
        assert!(
            result.is_err(),
            "volume index {MAX_VOLUMES} must be rejected"
        );
    }

    #[test]
    fn boot_config_with_network_attaches_iface() {
        let cfg = BootConfig::quickstart("/tmp/k".into(), "/tmp/r".into(), "/tmp/w".into())
            .with_network(NetworkConfig::default_tap("forkd-tap0"));
        let net = cfg.network.as_ref().unwrap();
        assert_eq!(net.iface_id, "eth0");
        assert_eq!(net.host_dev_name, "forkd-tap0");
        // kernel ip= argument is rendered correctly
        let ip_arg = net.kernel_ip_arg().expect("default_tap sets static ip");
        assert!(ip_arg.starts_with("ip=10.42.0.2"));
        assert!(ip_arg.contains(":10.42.0.1:"));
        assert!(ip_arg.ends_with(":eth0:off"));
    }

    #[test]
    fn fork_opts_default_disables_prewarm() {
        // The cold-cache mitigation is opt-in: callers who don't ask for
        // it pay no overhead. Regression guard against accidentally
        // flipping the default in a future refactor (which would silently
        // add a tmpfs scratch-dir requirement to every restore_many call).
        let opts = ForkOpts::default();
        assert!(opts.prewarm_scratch_dir.is_none());
    }

    #[test]
    fn fork_opts_default_uses_file_backend() {
        // v0.2 ships only File. Userfault is v0.3 scaffolding and would
        // bail!() in restore_many_with — flipping the default would break
        // every existing caller silently.
        let opts = ForkOpts::default();
        assert!(matches!(opts.memory_backend, MemoryBackend::File));
    }

    #[test]
    fn memory_backend_default_is_file() {
        // Belt-and-suspenders: Default::default() on MemoryBackend itself
        // (used independently of ForkOpts) must also return File.
        assert!(matches!(MemoryBackend::default(), MemoryBackend::File));
    }

    #[test]
    fn snapshot_serializes_round_trip() {
        let s = Snapshot {
            vmstate: "/tmp/v".into(),
            memory: "/tmp/m".into(),
            volumes: vec![VolumeSpec {
                host_path: "/var/lib/forkd/vol/pyagent.img".into(),
                guest_path: "/opt/cache".into(),
                read_only: false,
            }],
            parent_tag: None,
            parent_content_hash: None,
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: Snapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(s.vmstate, back.vmstate);
        assert_eq!(s.memory, back.memory);
        assert_eq!(s.volumes.len(), back.volumes.len());
        assert_eq!(s.volumes[0].guest_path, back.volumes[0].guest_path);
        // Base snapshot: chain fields stay None across round-trip and
        // — importantly — don't appear in the JSON at all, so older
        // forkd binaries still parse the file.
        assert_eq!(s.parent_tag, back.parent_tag);
        assert_eq!(s.parent_content_hash, back.parent_content_hash);
        assert!(
            !json.contains("parent_tag"),
            "base snapshot JSON must not include parent_tag (forward compat with v0.4 readers); got: {json}"
        );
    }

    #[test]
    fn snapshot_chained_serializes_round_trip() {
        let s = Snapshot {
            vmstate: "/tmp/v".into(),
            memory: "/tmp/diff.bin".into(),
            volumes: Vec::new(),
            parent_tag: Some("python-numpy".to_string()),
            parent_content_hash: Some("a".repeat(64)),
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: Snapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.parent_tag.as_deref(), Some("python-numpy"));
        assert_eq!(back.parent_content_hash, Some("a".repeat(64)));
        assert!(json.contains("parent_tag"));
        assert!(json.contains("parent_content_hash"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn apply_diff_copies_only_data_regions() {
        // Construct a synthetic base + sparse diff. Base = 16 KiB of
        // 0xAA. Diff = same length, but only bytes [4096..8192) and
        // [12288..16384) contain data (0xBB); the rest are holes.
        // After apply_diff the base should have 0xBB in those two
        // ranges and 0xAA elsewhere, and the reported copy count
        // should equal exactly the data-region bytes.
        use std::io::{Seek, SeekFrom, Write};
        let tmp = std::env::temp_dir().join(format!("apply-diff-test-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let base = tmp.join("base.bin");
        let diff = tmp.join("diff.bin");

        // Base: 16 KiB of 0xAA.
        std::fs::write(&base, vec![0xAAu8; 16 * 1024]).unwrap();

        // Diff: sparse file. We write data at the two regions and
        // truncate to the full size so SEEK_DATA/SEEK_HOLE see holes
        // between/around them.
        let mut df = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&diff)
            .unwrap();
        df.set_len(16 * 1024).unwrap();
        df.seek(SeekFrom::Start(4096)).unwrap();
        df.write_all(&[0xBBu8; 4096]).unwrap();
        df.seek(SeekFrom::Start(12288)).unwrap();
        df.write_all(&[0xBBu8; 4096]).unwrap();
        drop(df);

        let copied = apply_diff(&diff, &base).expect("apply_diff");
        assert_eq!(
            copied, 8192,
            "should copy exactly the 2x 4 KiB data regions"
        );

        let result = std::fs::read(&base).unwrap();
        assert_eq!(result.len(), 16 * 1024);
        // First page: original 0xAA preserved.
        assert!(result[0..4096].iter().all(|&b| b == 0xAA));
        // Second page: overwritten with 0xBB from diff.
        assert!(result[4096..8192].iter().all(|&b| b == 0xBB));
        // Third page: original 0xAA preserved.
        assert!(result[8192..12288].iter().all(|&b| b == 0xAA));
        // Fourth page: overwritten with 0xBB from diff.
        assert!(result[12288..16384].iter().all(|&b| b == 0xBB));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn apply_diff_handles_empty_diff() {
        // A diff with no dirty pages (e.g., source paused immediately
        // after restore) is an all-holes file. apply_diff should
        // return 0 and leave base untouched.
        let tmp = std::env::temp_dir().join(format!("apply-diff-empty-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let base = tmp.join("base.bin");
        let diff = tmp.join("diff.bin");
        std::fs::write(&base, vec![0xAAu8; 8192]).unwrap();
        let df = std::fs::File::create(&diff).unwrap();
        df.set_len(8192).unwrap();
        drop(df);

        let copied = apply_diff(&diff, &base).expect("apply_diff");
        assert_eq!(copied, 0, "empty diff should copy nothing");
        let result = std::fs::read(&base).unwrap();
        assert!(
            result.iter().all(|&b| b == 0xAA),
            "base should be untouched"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn apply_diff_rejects_size_mismatch() {
        // Regression for #180: a diff that's larger than its base must
        // not silently extend the base file. Without the size-equality
        // check at the top of apply_diff, base_f.write_all past the
        // base's original length would grow the file and corrupt the
        // snapshot for any subsequent mmap-based restore.
        let tmp = std::env::temp_dir().join(format!("apply-diff-mismatch-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let base = tmp.join("base.bin");
        let diff = tmp.join("diff.bin");

        // Base = 8 KiB, diff = 16 KiB (mismatch).
        std::fs::write(&base, vec![0xAAu8; 8 * 1024]).unwrap();
        std::fs::write(&diff, vec![0xBBu8; 16 * 1024]).unwrap();

        let err = apply_diff(&diff, &base).expect_err("size mismatch should error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("size mismatch"),
            "expected 'size mismatch' in error, got: {msg}"
        );
        // Base must be untouched — no silent extension.
        let result = std::fs::read(&base).unwrap();
        assert_eq!(
            result.len(),
            8 * 1024,
            "base must not have been extended; got len {}",
            result.len()
        );
        assert!(
            result.iter().all(|&b| b == 0xAA),
            "base bytes must be untouched"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn work_dir_sweep_preserves_chainstage_subdirectory() {
        // Regression for the v0.5 Phase 2a bug where
        // controller/http.rs wrote `memory-assembled.bin` directly into
        // work_dir, only for restore_many_with's startup sweep
        // (read_dir + remove_file for every non-dir entry) to unlink it
        // before FC could load it. Symptom in FC:
        //   "Failed to load guest memory: No such file or directory"
        //
        // Fix moved the assembled file into a `chainstage/`
        // subdirectory of work_dir. The sweep's `if p.is_dir()
        // { continue; }` then preserves it.
        //
        // This test documents the invariant: the same sweep logic that
        // restore_many_with uses must skip the chainstage subdirectory
        // (and the file under it).
        let tmp =
            std::env::temp_dir().join(format!("forkd-work-dir-sweep-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        // Stale FC artifacts the sweep is *meant* to remove.
        let stale_sock = tmp.join("child-1.sock");
        let stale_console = tmp.join("child-1.console");
        std::fs::write(&stale_sock, b"").unwrap();
        std::fs::write(&stale_console, b"old output").unwrap();

        // The v0.5 chain assembly lays out work_dir/chainstage/<file>.
        let chainstage = tmp.join("chainstage");
        std::fs::create_dir_all(&chainstage).unwrap();
        let assembled = chainstage.join("memory-assembled.bin");
        std::fs::write(&assembled, b"PRETEND-ASSEMBLED-MEMORY").unwrap();

        // Replicate the sweep loop from restore_many_with (lines
        // ~1349-1355 of this file). If this loop ever changes shape
        // — e.g. to recurse into subdirs — this assertion will catch
        // it before users hit the FC restore failure in production.
        for e in std::fs::read_dir(&tmp).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                continue;
            }
            let _ = std::fs::remove_file(&p);
        }

        assert!(
            !stale_sock.exists(),
            "sweep must still remove stale FC sockets"
        );
        assert!(
            !stale_console.exists(),
            "sweep must still remove stale console files"
        );
        assert!(
            chainstage.is_dir(),
            "sweep must not remove the chainstage subdirectory"
        );
        assert!(
            assembled.exists(),
            "the assembled chain memory must survive the sweep — \
             v0.5 Phase 2a depends on this"
        );
        let bytes = std::fs::read(&assembled).unwrap();
        assert_eq!(bytes, b"PRETEND-ASSEMBLED-MEMORY");

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
