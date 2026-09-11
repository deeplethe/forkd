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
use std::time::{Duration, Instant, SystemTime};

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
            // `random.trust_cpu=on` tells the guest kernel to trust
            // RDRAND for CRNG init — one of two paths out of the #218
            // entropy-starvation hang (the other being a virtio-rng
            // device, attached in `Vm::boot`). Requires Linux 4.19+
            // to be honored; forkd's shipped 4.14 vmlinux silently
            // ignores it but the option is forward-compatible. When
            // the kernel is updated this immediately resolves
            // `getrandom(2)`-blocking for OpenSSL workloads.
            // RDRAND is universal on x86_64 hosts forkd targets
            // (Intel since Ivy Bridge 2012, AMD since Zen 2017).
            // `clocksource=kvm-clock` forces the guest to use the KVM
            // paravirtualized clock, which always reflects the host's
            // wall clock time. Without this, the kernel boots with
            // kvm-clock but switches to TSC early in init — and when
            // sandboxes are created via snapshot restore, the TSC is
            // stale (frozen at the snapshot creation time). This causes
            // the guest wall clock to be wrong by days/weeks, breaking
            // TLS certificate validation and any time-sensitive logic.
            //
            // x86_64-only: kvm-clock (CONFIG_KVM_CLOCK) is an x86
            // paravirtualized clock. On aarch64 the parameter is
            // silently ignored (the kernel falls back to auto-select).
            //
            // Forward-only on the snapshot side: existing snapshots
            // created before this change have the kernel locked to
            // TSC and must be re-baked via `forkd from-image` before
            // restored sandboxes inherit the kvm-clock fix.
            boot_args:
                "console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda ro random.trust_cpu=on clocksource=kvm-clock".into(),
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
            // clocksource=kvm-clock: see quickstart() for rationale.
            // Forward-only on the snapshot side: existing snapshots
            // must be re-baked via `forkd from-image` to inherit this fix.
            boot_args:
                "console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda rw init=/forkd-init.sh random.trust_cpu=on clocksource=kvm-clock".into(),
            work_dir,
            rootfs_read_only: false,
            network: None,
            volumes: Vec::new(),
        }
    }

    /// Boot config for a **read-only ext4 rootfs with an in-guest writable
    /// layer** — the right shape for any snapshot that will be restored
    /// more than once.
    ///
    /// The ext4 is opened read-only on the host *and* mounted `ro` by the
    /// guest kernel, so nothing can write it; `/forkd-init.sh` provides
    /// tmpfs and overlayfs mounts in guest RAM for everything writable.
    /// That is what makes concurrent children safe: they share a base that
    /// is provably never written, instead of one ext4 opened read-write by
    /// every child. Firecracker freezes the drive's path and read-only flag
    /// in the vmstate and cannot re-point them at restore, so "never
    /// written" is the only property that can be enforced from the host.
    ///
    /// Writable state lives in guest memory, which `memory.bin` captures,
    /// so it survives a BRANCH — the same reason the guest's `/tmp` tmpfs
    /// always has.
    ///
    /// Guest RAM has to cover the writable footprint: the tmpfs is capped
    /// (2 GiB in the guest unless `with_rw_size` overrides it), so a job
    /// that outgrows it fails its own write with ENOSPC instead of
    /// silently corrupting the shared base or OOMing the guest.
    pub fn ext4_overlay(kernel: PathBuf, rootfs: PathBuf, work_dir: PathBuf) -> Self {
        Self {
            kernel,
            rootfs,
            vcpu_count: 2,
            mem_size_mib: 512,
            // `ro` is load-bearing, not cosmetic: it is what keeps the
            // shared ext4 byte-identical across every restored child.
            // clocksource/trust_cpu carry the same rationale as
            // quickstart(); `init=` is required because a read-only root
            // cannot let an image's own init run.
            boot_args:
                "console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda ro init=/forkd-init.sh random.trust_cpu=on clocksource=kvm-clock".into(),
            work_dir,
            rootfs_read_only: true,
            network: None,
            volumes: Vec::new(),
        }
    }

    /// Size cap for the guest's writable layer (overlay uppers plus
    /// scratch), appended as a `forkd.rw_size=` kernel cmdline hint that
    /// `/forkd-init.sh` reads. Takes a tmpfs size string, e.g. `"8g"`.
    /// Defaults to 2 GiB in the guest.
    pub fn with_rw_size(mut self, size: &str) -> Self {
        self.boot_args.push_str(&format!(" forkd.rw_size={size}"));
        self
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

/// Accept one connection on an already-non-blocking `UnixListener`,
/// giving up at `deadline`. Prevents a peer that never connects from
/// wedging a blocking `accept()` (and therefore the caller's `join()`)
/// forever — see `request_wp_uffd` (#261). The returned stream is put
/// back into blocking mode for ordinary reads.
#[cfg(target_os = "linux")]
fn accept_with_deadline(
    listener: &std::os::unix::net::UnixListener,
    deadline: Instant,
) -> Result<std::os::unix::net::UnixStream> {
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream
                    .set_nonblocking(false)
                    .context("restore blocking on accepted stream")?;
                return Ok(stream);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    anyhow::bail!("peer did not connect to the socket before the deadline");
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e) => return Err(e).context("accept connection"),
        }
    }
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
        //
        // The listener is MOVED into this thread, so it is NOT dropped on
        // the error paths below — if FC accepts the PUT but never dials
        // back (or the PUT itself fails), a plain blocking `accept()`
        // would wedge `join()` forever. Bound it with a deadline that
        // outlives the PUT timeout so a non-connecting FC surfaces as a
        // clean error instead of a permanent hang (#261).
        listener
            .set_nonblocking(true)
            .context("set WP-uffd UDS non-blocking")?;
        let accept_thread =
            std::thread::spawn(move || -> Result<forkd_uffd::handshake::Handshake> {
                let deadline = Instant::now() + Duration::from_secs(DEFAULT_API_TIMEOUT_SECS + 2);
                let stream = accept_with_deadline(&listener, deadline)
                    .context("waiting for Firecracker to connect to the WP-uffd socket")?;
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

        // Join the accept thread (even on PUT failure). It self-terminates
        // on its own deadline, so this can't hang even if FC never
        // connected — the previous "listener drops at end of scope"
        // reasoning was wrong (the listener is owned by the thread).
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

/// A firecracker process found on the host that may interfere with a
/// restore (holding a tap device fd, a netns slot, or consuming fd/RAM
/// budget). Returned by [`scan_firecracker_orphans`].
#[derive(Debug, Clone)]
pub struct OrphanProcess {
    pub pid: u32,
    /// Elapsed seconds since the process started (best-effort, from
    /// `/proc/<pid>/stat` start time). 0 when unavailable.
    pub elapsed_secs: u64,
    /// The command line (best-effort, from `/proc/<pid>/cmdline`).
    /// Empty when unreadable.
    pub cmdline: String,
}

/// Scan `/proc` for running firecracker processes whose PIDs are NOT in
/// the `known_pids` set. These are potential orphans — processes from a
/// prior (crashed) run that still hold tap devices, netns slots, or fd
/// budget, and may doom a new `restore_many_with` to socket/restore
/// failures. The caller (controller) passes the PIDs of every VM in
/// `live_vms` so only untracked firecracker processes are reported.
///
/// This is a best-effort, read-only scan: it never kills anything. The
/// controller logs the results before a restore so operators can see
/// contention from orphans. The scan is one `/proc` readdir plus, for
/// each firecracker PID found, reads of `/proc/<pid>/comm`,
/// `/proc/<pid>/cmdline`, and `/proc/<pid>/stat` (via `proc_elapsed_secs`).
/// Safe to call before every restore — the per-orphan I/O is bounded by
/// the number of firecracker processes on the host (typically small).
///
/// TOCTOU note: the scan is a point-in-time observation. New firecracker
/// processes may appear between scan and restore, and a known PID could
/// be reused by a new process in that window. The scan is logging-only
/// and never gates a restore, so this is acceptable, but operators should
/// not treat the scan as a guarantee that no orphans exist at restore time.
///
/// Returns an empty vec on non-Linux platforms (no `/proc`).
pub fn scan_firecracker_orphans(known_pids: &std::collections::HashSet<u32>) -> Vec<OrphanProcess> {
    scan_firecracker_orphans_impl(known_pids)
}

#[cfg(target_os = "linux")]
fn scan_firecracker_orphans_impl(
    known_pids: &std::collections::HashSet<u32>,
) -> Vec<OrphanProcess> {
    let mut orphans = Vec::new();
    let entries = match std::fs::read_dir("/proc") {
        Ok(e) => e,
        Err(_) => return orphans,
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = match name.to_str() {
            Some(s) => s,
            None => continue,
        };
        let pid: u32 = match name_str.parse() {
            Ok(p) => p,
            Err(_) => continue,
        };
        if pid <= 1 || known_pids.contains(&pid) {
            continue;
        }
        // `/proc/<pid>/comm` is the process's command name (truncated to
        // 15 chars). For firecracker this is exactly "firecracker".
        let comm = match std::fs::read_to_string(format!("/proc/{pid}/comm")) {
            Ok(s) => s.trim().to_string(),
            Err(_) => continue,
        };
        if comm != "firecracker" {
            continue;
        }
        // Best-effort cmdline + elapsed. Failure here doesn't disqualify
        // the orphan — we already confirmed it's a firecracker process.
        let cmdline = std::fs::read_to_string(format!("/proc/{pid}/cmdline"))
            .unwrap_or_default()
            .replace('\0', " ")
            .trim()
            .to_string();
        let elapsed_secs = proc_elapsed_secs(pid).unwrap_or(0);
        orphans.push(OrphanProcess {
            pid,
            elapsed_secs,
            cmdline,
        });
    }
    orphans
}

/// Read `/proc/<pid>/stat` and compute elapsed seconds since process start.
/// Returns 0 when the file is missing or unparseable (non-fatal).
#[cfg(target_os = "linux")]
fn proc_elapsed_secs(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Field 22 is starttime in clock ticks since boot. The comm field (2)
    // is parenthesized and may contain spaces, so split from the right.
    let rparen = stat.rfind(')')?;
    let rest = &stat[rparen + 1..];
    let mut fields = rest.split_whitespace();
    // Fields 3..=21 (state through itrealvalue) precede field 22 (starttime).
    for _ in 0..19 {
        fields.next();
    }
    let starttime_ticks: u64 = fields.next()?.parse().ok()?;
    let start_secs = starttime_ticks / PROC_CLK_TCK;
    let now = boot_relative_secs()?;
    now.checked_sub(start_secs)
}

/// Clock ticks per second for `/proc/<pid>/stat` time fields. This is
/// `USER_HZ` on Linux, which is 100 on every x86_64/aarch64 host forkd
/// targets today. `libc::sysconf(libc::_SC_CLK_TCK)` could read it at
/// runtime, but the value is effectively fixed across all supported
/// kernels — hardcoding avoids a runtime probe for a constant.
#[cfg(target_os = "linux")]
const PROC_CLK_TCK: u64 = 100;

/// Seconds since boot, from `/proc/uptime`. None when unreadable.
#[cfg(target_os = "linux")]
fn boot_relative_secs() -> Option<u64> {
    let uptime = std::fs::read_to_string("/proc/uptime").ok()?;
    let first = uptime.split_whitespace().next()?;
    let secs: f64 = first.parse().ok()?;
    Some(secs as u64)
}

#[cfg(not(target_os = "linux"))]
fn scan_firecracker_orphans_impl(
    _known_pids: &std::collections::HashSet<u32>,
) -> Vec<OrphanProcess> {
    // No `/proc` on non-Linux — nothing to scan. This stub keeps the
    // function callable from dev boxes (macOS) for test compilation.
    Vec::new()
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
    /// Absolute path to the rootfs block image this snapshot's vmstate
    /// was frozen against. Firecracker bakes this path into the vmstate
    /// and reopens it verbatim at restore, so a snapshot only restores
    /// on a host where this exact path exists. Recorded here (v0.5.3+)
    /// so `pack` / `pull` can ship the rootfs as a content-addressed
    /// sidecar and place it back at this path on the puller — see
    /// `forkd pack` / issue #242. `None` for snapshots written before
    /// this field existed, and for daemon-side branches that inherit
    /// the source's rootfs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rootfs: Option<PathBuf>,
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
    ///
    /// `use_hugepages`: when true, the memfd is backed with 2 MiB
    /// hugepages (`MFD_HUGETLB | MFD_HUGE_2MB`). Reduces TLB pressure
    /// during spawn-many and the BRANCH bulk-copy pass. Requires the
    /// host hugepage pool to be non-empty (`HugePages_Free > 0` in
    /// `/proc/meminfo`); `forkd doctor` checks this. Falls back to
    /// normal 4 KiB pages with a warning if the pool is exhausted.
    MemfdShared {
        use_hugepages: bool,
    },
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
    /// Per-child timeout for waiting on a firecracker API socket to appear
    /// after spawn, in seconds. The default of 10s matches the historical
    /// hardcoded budget. Under contention from orphaned VMs, parallel restore
    /// bursts, or slow tap/netns teardown, firecracker processes can take
    /// longer than 10s to open their API socket; callers that expect heavy
    /// load (e.g. the daemon's warm-pool refiller under spawn bursts) may
    /// raise this to give each child more room rather than fail the whole
    /// batch. Set to 0 to use the default. See `wait_for_sock`.
    pub socket_wait_timeout_secs: u64,
    /// If true, after snapshot restore each child's guest clock is
    /// synchronized to host wall time by exec'ing `date -s @<ts>`
    /// via the guest agent (port 8888). This is the general fix for
    /// stale guest clocks on older kernels (< 5.16) where
    /// KVM_CLOCK_REALTIME is unavailable and the restored kvm-clock
    /// stays at the snapshot's creation time.
    ///
    /// Default false: snapshots without forkd-agent (custom images)
    /// would add up to 10 seconds per restore waiting for an agent
    /// that never starts. Callers that know their images contain
    /// forkd-agent (e.g. the daemon's spawn paths) set this to true.
    pub sync_guest_clocks: bool,
}

/// The historical hardcoded socket-wait budget, in seconds. Used as the
/// default when `ForkOpts::socket_wait_timeout_secs` is 0.
pub const DEFAULT_SOCKET_WAIT_SECS: u64 = 10;

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
            socket_wait_timeout_secs: DEFAULT_SOCKET_WAIT_SECS,
            sync_guest_clocks: false,
        }
    }
}

/// Result of `Snapshot::restore_many` — N live children plus timing.
#[derive(Debug)]
pub struct ForkResult {
    /// Successfully restored children, each with its 1-based within-batch
    /// `child_index` so callers can map a surviving VM back to its
    /// resource slot (netns/cgroup at `netns_offset + child_index`).
    /// Failed children are NOT here — see `failures`.
    pub children: Vec<ForkChild>,
    /// Per-child failures collected during spawn, socket-wait, and
    /// restore. Empty when every child succeeded. When non-empty,
    /// `children` holds the successful subset (possibly empty) and the
    /// caller decides whether to retry the failed indices or degrade.
    /// `child_index` is the 1-based within-batch index, so a caller with
    /// per-child resources (netns/cgroup at `netns_offset + child_index`)
    /// can release exactly the failed slots.
    pub failures: Vec<RestoreFailure>,
    pub spawn_ms: u128,
    pub restore_ms: u128,
    /// Wall-clock spent in the optional post-restore prewarm pass.
    /// Zero when `prewarm_scratch_dir` was None.
    pub prewarm_ms: u128,
    /// Wall-clock spent in the best-effort guest clock sync pass
    /// (Phase 2.5). Zero when no children needed syncing or when
    /// the sync timed out immediately. Non-fatal: a stale clock
    /// does not prevent the sandbox from being usable.
    pub clock_sync_ms: u128,
    /// Per-child guest clock sync outcomes (empty when clock sync
    /// was not requested via `ForkOpts::sync_guest_clocks`).
    /// Callers can inspect this to decide whether to fail-closed
    /// or degrade when some children have unsynchronized clocks.
    /// The count of `Synced` entries should match the number of
    /// children; any `Failed` entry indicates a stale clock that
    /// may break TLS certificate validation or token expiry.
    pub clock_sync_outcomes: Vec<ClockSyncOutcome>,
}

/// Per-child outcome of the best-effort guest clock sync pass.
/// Exposed via `ForkResult::clock_sync_outcomes` so callers can
/// fail-closed or degrade when some children have unsynchronized
/// clocks. A stale guest clock breaks TLS certificate validation
/// and token expiry checks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClockSyncOutcome {
    /// The guest clock was successfully set to host wall time.
    Synced { child_index: usize },
    /// The guest agent did not respond within the timeout, or the
    /// `date -s` exec failed. The sandbox is usable but has a stale
    /// clock. `error` describes what went wrong.
    Failed { child_index: usize, error: String },
}

/// A successfully restored child, paired with its 1-based within-batch
/// index so callers can map it back to per-child resources (netns/cgroup
/// at `netns_offset + child_index`). This preserves the child-to-VM
/// mapping that a plain `Vec<Vm>` would lose when failures are filtered
/// out (review feedback on #301: successes must retain their index).
#[derive(Debug)]
pub struct ForkChild {
    /// 1-based within-batch child index (matches `child-{index}.sock`).
    pub child_index: usize,
    /// The restored VM handle (owns the firecracker Child process).
    pub vm: Vm,
}

/// Which phase of `restore_many_with` a child failed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestorePhase {
    /// `spawn_firecracker_in` returned an error before the process existed.
    Spawn,
    /// The firecracker API socket never appeared within the wait budget.
    SocketWait,
    /// `PUT /snapshot/load` returned a non-2xx response (or a transport error).
    Restore,
}

/// A single child's failure inside `restore_many_with`. Collected per
/// child so a single bad spawn doesn't mask other failures (or silently
/// drop already-spawned siblings). `pid` is `None` for `Spawn`-phase
/// failures where the process was never created.
#[derive(Debug, Clone)]
pub struct RestoreFailure {
    /// 1-based within-batch child index (matches `child-{index}.sock`).
    pub child_index: usize,
    /// The phase that failed.
    pub phase: RestorePhase,
    /// Firecracker PID when known (None for Spawn-phase failures).
    pub pid: Option<u32>,
    /// Human-readable error from the failing phase.
    pub error: String,
}

/// Error returned by `restore_many_with` when one or more children fail.
/// Carries every per-child failure so callers can report which specific
/// child failed rather than a single bail that loses the batch context.
/// Children that spawned successfully but could not be restored are
/// killed (firecracker SIGKILL'd) before this is returned, so no live
/// orphans remain from a partial failure.
#[derive(Debug, Clone)]
pub struct RestoreError {
    pub failures: Vec<RestoreFailure>,
}

impl std::fmt::Display for RestoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "restore_many_with failed: {} child(ren) failed",
            self.failures.len()
        )?;
        for fail in &self.failures {
            write!(
                f,
                "; child {} {:?}: {}",
                fail.child_index, fail.phase, fail.error
            )?;
            if let Some(pid) = fail.pid {
                write!(f, " (pid {})", pid)?;
            }
        }
        Ok(())
    }
}

impl std::error::Error for RestoreError {}

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

/// Get the host kernel release string (e.g. "6.1.0-foo").
/// Used by tests to determine whether KVM_CLOCK_REALTIME is available
/// (needs kernel ≥ 5.16). Returns `None` if `/proc/sys/kernel/osrelease`
/// cannot be read (non-Linux).
#[cfg(test)]
fn host_kernel_release() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .ok()
            .map(|s| s.trim().to_string())
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Parse a kernel release string (e.g. "5.15.0-foo", "6.1.0-bar") and
/// return whether the kernel supports `KVM_CLOCK_REALTIME` (≥ 5.16).
/// On x86 this makes kvm-clock auto-correct to host wall time on
/// snapshot restore; on older kernels the restored kvm-clock stays
/// stale and needs a complementary guest-side correction (PR #300).
#[cfg(test)]
fn kernel_supports_kvm_clock_realtime(release: &str) -> bool {
    // Parse the first two numeric components of the release string.
    let parts: Vec<&str> = release.split('.').collect();
    if parts.len() < 2 {
        return false;
    }
    let major: u32 = parts[0].parse().unwrap_or(0);
    let minor: u32 = parts[1].parse().unwrap_or(0);
    major > 5 || (major == 5 && minor >= 16)
}

/// The default guest agent address. Each child VM's network is
/// configured with guest_ip 10.42.0.2 and the agent listens on port
/// 8888 (see `forkd-agent.py`). When children live in separate netns
/// (per_child_netns = true), they all share this address — each netns
/// is its own broadcast domain so there is no conflict.
const GUEST_AGENT_ADDR: &str = "10.42.0.2:8888";

/// Maximum time to wait for the guest agent to come up after restore.
/// The agent is forkd-agent.py running as PID 1 inside the guest. It
/// starts as soon as the kernel finishes booting and init runs, which
/// for snapshot-restored VMs is typically sub-second. The 10-second
/// budget covers slow hosts, cold cache, and large rootfs images.
const CLOCK_SYNC_AGENT_TIMEOUT: Duration = Duration::from_secs(10);

/// Per-call timeout for the `date -s` exec itself.
const CLOCK_SYNC_EXEC_TIMEOUT: Duration = Duration::from_secs(5);

/// Best-effort guest clock synchronization after snapshot restore.
///
/// # Motivation
///
/// Firecracker restores the guest's KVM clock from the snapshot's
/// saved clock state, not the host's current wall time. For snapshots
/// created minutes or hours ago, the guest clock starts in the past
/// and advances from there. This causes:
///
/// - TLS certificate validation failures (certs appear \"not yet valid\"
///   or expired relative to the guest's stale clock)
/// - Incorrect timestamp logging
/// - Token / credential expiry checks failing inside the guest
///
/// The `clocksource=kvm-clock` kernel boot arg (PR #289) forces the
/// guest to use host-synced kvm-clock instead of TSC, but it has
/// limitations:
/// - **x86-only**: `kvm-clock` is an x86 clocksource, silently ignored
///   on aarch64.
/// - **Forward-only**: boot args are not re-applied on snapshot restore
///   (Firecracker's restore path only issues `PUT /snapshot/load`, never
///   `PUT /boot-source`). Existing snapshots must be re-baked.
/// - **Older kernels**: On Linux < 5.16, `KVM_CLOCK_REALTIME` is
///   unavailable, so `KVM_SET_CLOCK` restores the saved clock rather
///   than host wall time.
///
/// This function provides a cross-platform, cross-kernel-version
/// alternative: after restore, poll the guest agent until it's alive,
/// then exec `date -s @<host_unix_ts>` to set the guest clock to the
/// host's current wall time. This works on any architecture and any
/// Linux kernel because it operates entirely in guest userspace.
///
/// # Alternatives considered
///
/// 1. **`clocksource=kvm-clock` boot arg** (PR #289): x86-only,
///    forward-only (requires re-baking snapshots), needs Linux ≥ 5.16
///    for full wall-clock correctness. Good as defense-in-depth on x86
///    but insufficient as the sole fix.
///
/// 2. **NTP/chrony in sandbox images**: Works on any platform but
///    requires network egress (blocked by restricted network policy),
///    adds image dependencies, and takes 1-5 seconds to converge.
///    Not reliable for CI sandboxes with `none` or `restricted` policy.
///
/// 3. **forkd-agent.py host-time injection via kernel cmdline**:
///    Pass `forkd.clock_realtime=<ts>` as a boot arg. But boot args
///    are snapshot-forward-only (same limitation as kvm-clock). Could
///    also pass via the agent's TCP protocol, but that has the same
///    polling window as this approach with more complexity.
///
/// 4. **KVM_CLOCK_REALTIME (Linux 5.16+)**: Firecracker-level change
///    to use `KVM_SET_CLOCK` with the `KVM_CLOCK_REALTIME` flag to
///    restore host wall clock. x86-only, requires recent host kernel,
///    and needs Firecracker support that may not exist.
///
/// This function (Option B from issue #288) is the most portable:
/// it works on any architecture, any Linux kernel, and any existing
/// snapshot without re-baking.
///
/// # Pros
///
/// - **Architecture-agnostic**: works on x86, aarch64, and any
///   architecture that runs Linux + a `date` binary.
/// - **Kernel-version-agnostic**: no dependency on KVM clock features.
/// - **Works on existing snapshots**: no re-baking needed.
/// - **Simple**: reuses the existing exec/ping guest agent protocol.
/// - **Non-fatal**: best-effort — failures are logged and the sandbox
///   remains usable with a stale clock.
///
/// # Cons
///
/// - **Brief wrong-clock window**: between restore and the `date -s`
///   exec, the guest clock is stale (~1-5 seconds). For TLS use cases,
///   callers should retry on certificate errors or the agent could
///   retry the HTTPS call after clock sync.
/// - **Requires guest agent**: if forkd-agent.py is not installed in
///   the image (e.g. a minimal custom rootfs), clock sync silently
///   fails. The sandbox is still usable.
/// - **Requires `date` binary**: if the image lacks `date` (coreutils
///   or busybox), the exec fails. This is extremely unlikely for any
///   Linux image but is handled gracefully.
/// - **Adds latency to restore**: ~1-5 seconds per batch (agent polling
///   + exec), running in parallel across children. Measured at
///     ~1-2 seconds for typical Debian/Ubuntu-based images.
///
/// Map a `Vec<Option<T>>` of per-slot children (failed slots are `None`)
/// into a slice of `(original_1_based_slot, &T)` for the live children,
/// where `original_1_based_slot` is the index the child occupied in the
/// *original* batch (1-based) — **before** failures were filtered out.
///
/// This is the single source of truth for the index-preserving mapping
/// that `restore_many_with` passes into `sync_guest_clocks`. Hardcoding
/// what would otherwise be an inline `enumerate().filter_map(...)` makes
/// the invariant unit-testable without KVM: a hole (a `None` slot)
/// earlier in the batch must not renumber a later survivor's index
/// (review feedback on #302 — `children::flatten()` loses the slot).
///
/// Example: `[None, Some(a), None, Some(b)] -> [(2, a), (4, b)]`.
pub fn live_children_with_indices<T>(children: &[Option<T>]) -> Vec<(usize, &T)> {
    children
        .iter()
        .enumerate()
        .filter_map(|(i, c)| c.as_ref().map(|vm| (i + 1, vm)))
        .collect()
}

/// Sync each restored child's guest clock to host wall time.
///
/// # Indexing contract
///
/// `children` is a slice of `(child_index, &Vm)` where `child_index` is
/// the **1-based within-batch slot** the child originally occupied in
/// `restore_many_with` (matching `ForkChild.child_index` and
/// `RestoreFailure.child_index`). `ClockSyncOutcome.child_index` echoes
/// that original slot.
///
/// Pass the live children flattened from `Vec<Option<Vm>>` only *after*
/// pairing each with its original index; do **not** re-enumerate the
/// flattened slice, otherwise a hole (failed spawn/socket-wait) shifts
/// every later survivor's reported index and breaks resource correlation
/// (review feedback on #302: `children::flatten()` renumbers outcomes).
fn sync_guest_clocks(children: &[(usize, &Vm)]) -> (u128, Vec<ClockSyncOutcome>) {
    let start = Instant::now();

    let mut handles = Vec::with_capacity(children.len());
    for child in children.iter() {
        // `child.1` is the `&Vm`; borrow fields before moving netns into
        // the spawn closure. (`child.0`'s original slot is reported via
        // `children[i].0` in the collect loop below.)
        let (_, vm) = *child;
        let netns = vm.netns.clone();
        handles.push(thread::spawn(move || -> Result<()> {
            let addr = GUEST_AGENT_ADDR.to_string();

            // Readiness probe: poll the guest agent until it responds
            // or the timeout expires. The agent (forkd-agent.py) starts
            // as PID 1 and begins listening on port 8888 as soon as init
            // runs. For snapshot-restored VMs, the guest is already past
            // kernel boot — only the agent's TCP listener needs to come
            // up.
            //
            // The probe is a closure so the timestamp-placement logic in
            // `sync_one_child_clock` can be unit-tested with a
            // deterministic delayed-readiness probe (see
            // `sync_one_child_clock_timestamp_fresh_after_delayed_ping`).
            let ping = || match &netns {
                Some(ns) => ping_in_netns(ns, addr.clone()).map(|_| ()),
                None => ping_at(&addr).map(|_| ()),
            };

            // Timestamp source: host wall time. Captured per child,
            // AFTER ping succeeds — see `sync_one_child_clock`.
            let now = || {
                SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs()
            };

            // Clock-setter: invoke `sh -c "date -s @<ts>"` via the guest
            // agent's exec. We use `sh -c` because the guest agent's exec
            // handler passes args directly to `subprocess.run(args)` — a
            // bare `["-c", cmd]` would try to execute a program named
            // "-c" and fail.
            //
            // `date -s @<ts>` is supported by GNU coreutils date (≥ 8.5)
            // and busybox date (with FEATURE_DATE_COMPAT). All forkd
            // images are built from Debian/Ubuntu Docker bases, so GNU
            // date is guaranteed.
            let set_clock = |ts: u64| {
                let date_cmd = format!("date -s @{ts}");
                let exec_result = match &netns {
                    Some(ns) => exec_in_netns(
                        ns,
                        addr.clone(),
                        vec!["sh".into(), "-c".into(), date_cmd],
                        CLOCK_SYNC_EXEC_TIMEOUT,
                    ),
                    None => exec_at(
                        &addr,
                        vec!["sh".into(), "-c".into(), date_cmd],
                        CLOCK_SYNC_EXEC_TIMEOUT,
                    ),
                };
                match exec_result {
                    Ok(resp) if resp.exit_code == 0 => Ok(()),
                    Ok(resp) => bail!(
                        "date -s exited with {}: {}",
                        resp.exit_code,
                        resp.stderr.trim()
                    ),
                    Err(e) => Err(e),
                }
            };

            sync_one_child_clock(ping, now, set_clock, CLOCK_SYNC_AGENT_TIMEOUT)
                .map_err(|e| e.context(format!("guest agent at {addr} did not come up")))
        }));
    }

    // Collect per-child outcomes. Each failure is logged as a
    // warning — clock sync is best-effort and must not prevent the
    // sandbox from being usable with a stale clock. The outcomes are
    // returned to the caller so it can fail-closed or degrade.
    let mut outcomes = Vec::with_capacity(handles.len());
    let mut synced = 0usize;
    let mut failed = 0usize;
    for (i, h) in handles.into_iter().enumerate() {
        // `children[i].0` is the ORIGINAL 1-based within-batch slot the
        // child occupied before failures were filtered out. Use it for
        // both the tracing spans and `ClockSyncOutcome.child_index` so a
        // survivor is never re-numbered by flattening (review #302).
        let slot = children[i].0;
        match h.join() {
            Ok(Ok(())) => {
                synced += 1;
                outcomes.push(ClockSyncOutcome::Synced { child_index: slot });
            }
            Ok(Err(e)) => {
                failed += 1;
                let error = format!("{e:#}");
                tracing::warn!(
                    child = slot,
                    "guest clock sync failed: {error}; \
                     sandbox will have a stale clock"
                );
                outcomes.push(ClockSyncOutcome::Failed {
                    child_index: slot,
                    error,
                });
            }
            Err(_) => {
                failed += 1;
                tracing::warn!(
                    child = slot,
                    "guest clock sync thread panicked; \
                     sandbox will have a stale clock"
                );
                outcomes.push(ClockSyncOutcome::Failed {
                    child_index: slot,
                    error: "clock sync thread panicked".to_string(),
                });
            }
        }
    }

    if synced > 0 {
        tracing::info!(
            synced,
            failed,
            elapsed_ms = start.elapsed().as_millis(),
            "synced guest clocks after restore"
        );
    }

    (start.elapsed().as_millis(), outcomes)
}

/// Synchronize one child's guest clock to host wall time.
///
/// This is the core timestamp-placement logic, extracted from
/// `sync_guest_clocks` so it can be unit-tested with injectable
/// readiness probe / timestamp source / clock-setter closures — no
/// VM, guest agent, or KVM required.
///
/// # The property under test
///
/// The host timestamp MUST be captured **after** the readiness probe
/// (`ping`) succeeds, immediately before `set_clock`. The earlier
/// implementation captured it **before** polling; a child whose agent
/// became ready near the 10-second deadline was set ~10 seconds in the
/// past, breaking TLS certificate validation and token expiry.
///
/// Capturing per-child after ping minimizes the gap between timestamp
/// capture and the `date -s` exec, so the guest clock matches host wall
/// time to within the (sub-second) ping→exec round-trip.
///
/// # Arguments
///
/// - `ping` — readiness probe. Called repeatedly until it returns `Ok`
///   or `timeout` elapses. In production this polls the guest agent at
///   `GUEST_AGENT_ADDR`; in tests it is a deterministic closure that
///   fails for a fixed duration then succeeds, so the induced poll delay
///   exceeds the accepted clock drift and the timestamp-freshness
///   assertion is meaningful.
/// - `now` — timestamp source (host wall time, seconds since UNIX
///   epoch). Injected so tests can pin time.
/// - `set_clock` — invokes `date -s @<ts>` via the guest agent. In
///   tests this records the timestamp passed so the test can assert
///   freshness.
/// - `timeout` — maximum time to wait for `ping` to succeed. Injected
///   so the timeout path can be unit-tested without waiting the full
///   10-second production budget.
fn sync_one_child_clock(
    mut ping: impl FnMut() -> Result<()>,
    now: impl Fn() -> u64,
    set_clock: impl FnOnce(u64) -> Result<()>,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if ping().is_ok() {
            break;
        }
        if Instant::now() >= deadline {
            bail!("did not respond within {}s", timeout.as_secs());
        }
        thread::sleep(Duration::from_millis(200));
    }

    // Capture the host timestamp AFTER the agent is confirmed alive,
    // immediately before set_clock. The old approach captured it before
    // polling — a child that became ready near the 10-second deadline
    // was set ~10 seconds in the past.
    let ts = now();
    set_clock(ts)
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
    let mut sleep_for = Duration::from_millis(1);
    let max_sleep = Duration::from_millis(20);
    while start.elapsed() < timeout {
        // Check for a Unix-domain socket, not just path existence —
        // a directory or regular file at the socket path must NOT
        // satisfy the wait (review feedback on #301: a pre-created
        // directory was incorrectly passing the check).
        if sock.is_dir() {
            bail!(
                "socket path {} is a directory, not a socket",
                sock.display()
            );
        }
        if sock.exists() {
            return Ok(());
        }
        let remaining = timeout.saturating_sub(start.elapsed());
        thread::sleep(std::cmp::min(sleep_for, remaining));
        sleep_for = std::cmp::min(sleep_for * 2, max_sleep);
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

        wait_for_sock(&sock, Duration::from_secs(10))?;

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

        // Attach a virtio-rng device. Empty body = no rate limiter
        // (FC reads from the host's /dev/urandom as fast as the
        // guest drains).
        //
        // This is the **device-side half** of fixing #218 (guest
        // CRNG never initializes → `getrandom(2)` blocks → anything
        // that touches OpenSSL hangs). The other half — the guest
        // kernel binding /dev/hwrng to this device — requires the
        // kernel image to be built with `CONFIG_HW_RANDOM_VIRTIO=y`.
        // forkd's shipped 4.14 vmlinux lacks that, so for now this
        // PUT just adds an inert virtio-mmio device on the bus.
        // When the kernel is updated (tracked separately) the
        // device immediately starts feeding the guest CRNG and the
        // issue clears with no further code change here.
        //
        // Forward-only on the snapshot side: existing snapshot
        // vmstate files won't have the entropy device until they're
        // re-baked via `forkd from-image`.
        api_call(&sock, "PUT", "/entropy", "{}")?;

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
            rootfs: None,
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
            rootfs: None,
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
                socket_wait_timeout_secs: DEFAULT_SOCKET_WAIT_SECS,
                sync_guest_clocks: false,
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
            MemoryBackend::File | MemoryBackend::MemfdShared { .. } => {}
            MemoryBackend::Userfault { .. } => bail!(
                "MemoryBackend::Userfault is v0.3 scaffolding and not yet \
                 implemented — see docs/design/userfaultfd.md for status"
            ),
        }
        let n = opts.n;
        // Reject n>1 with per_child_netns=false: all children would
        // share the same guest IP (10.42.0.2) and MAC, so the guest
        // agent (port 8888) and all guest network traffic are
        // ambiguous — requests can hit one VM while siblings remain
        // unsynchronized. This topology conflict is fundamental, not
        // a configuration issue. Use per_child_netns=true for n>1.
        if n > 1 && !opts.per_child_netns {
            bail!(
                "n>1 with per_child_netns=false is not supported: all children \
                 share the same guest IP/MAC, making per-child addressing impossible. \
                 Use per_child_netns=true for multi-child spawns."
            );
        }
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
        let mut children: Vec<Option<Vm>> = Vec::with_capacity(n);
        let mut failures: Vec<RestoreFailure> = Vec::new();
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
            match spawn_firecracker_in(netns.as_deref(), &sock, &console) {
                Ok(proc) => {
                    let pid = proc.id();
                    children.push(Some(Vm {
                        proc,
                        pid,
                        sock,
                        console,
                        netns,
                        cgroup: None,
                        memfd: None,
                    }));
                }
                Err(e) => {
                    // Record the failure but keep spawning the rest so a
                    // single bad spawn doesn't mask other failures. The
                    // successful siblings are returned (partial success).
                    failures.push(RestoreFailure {
                        child_index: i,
                        phase: RestorePhase::Spawn,
                        pid: None,
                        error: format!("{e:#}"),
                    });
                    children.push(None);
                }
            }
        }
        let socket_wait = Duration::from_secs(if opts.socket_wait_timeout_secs == 0 {
            DEFAULT_SOCKET_WAIT_SECS
        } else {
            opts.socket_wait_timeout_secs
        });
        // Concurrent socket wait: one thread per spawned child, all with
        // the SAME timeout so the batch is bounded by one timeout, not
        // n × timeout. A failed child is dropped (killing its firecracker
        // process) without disturbing siblings — partial success.
        let sock_handles: Vec<(usize, thread::JoinHandle<Result<()>>)> = children
            .iter()
            .enumerate()
            .filter_map(|(i, slot)| {
                let vm = slot.as_ref()?;
                let sock = vm.sock.clone();
                let pid = vm.pid;
                Some((
                    i,
                    thread::spawn(move || -> Result<()> {
                        let wait_start = Instant::now();
                        wait_for_sock(&sock, socket_wait)?;
                        let elapsed = wait_start.elapsed();
                        // Warn when a child takes >80% of the timeout to
                        // appear — an early sign of contention (orphaned
                        // VMs, tap/netns teardown races, fd pressure) that
                        // may soon push the next spawn over.
                        if elapsed > socket_wait * 4 / 5 {
                            tracing::warn!(
                                pid,
                                sock = %sock.display(),
                                elapsed_ms = elapsed.as_millis() as u64,
                                timeout_ms = socket_wait.as_millis() as u64,
                                "firecracker socket appeared near the wait budget; \
                                 load contention may cause future spawns to time out"
                            );
                        }
                        Ok(())
                    }),
                ))
            })
            .collect();
        for (i, h) in sock_handles {
            let pid = children[i].as_ref().map(|vm| vm.pid);
            match h.join() {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    failures.push(RestoreFailure {
                        child_index: i + 1,
                        phase: RestorePhase::SocketWait,
                        pid,
                        error: format!("{e:#}"),
                    });
                    // Dropping the child kills its firecracker process.
                    children[i] = None;
                }
                Err(panic) => {
                    failures.push(RestoreFailure {
                        child_index: i + 1,
                        phase: RestorePhase::SocketWait,
                        pid,
                        error: format!("socket-wait thread panicked: {panic:?}"),
                    });
                    children[i] = None;
                }
            }
        }
        let spawn_ms = spawn_start.elapsed().as_millis();

        // Optional: place each child in its own cgroup v2 leaf with a
        // memory.max limit. We do this AFTER the socket is up but BEFORE
        // restore so the limit applies to the memory image mmap as well.
        if let Some(mib) = opts.memory_limit_mib {
            let bytes = cgroup::mib_to_bytes(mib);
            for (i, slot) in children.iter_mut().enumerate() {
                let Some(child) = slot.as_mut() else {
                    continue;
                };
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
        if matches!(opts.memory_backend, MemoryBackend::MemfdShared { .. }) {
            let use_hugepages = matches!(
                opts.memory_backend,
                MemoryBackend::MemfdShared {
                    use_hugepages: true
                }
            );
            for (i, slot) in children.iter_mut().enumerate() {
                let Some(child) = slot.as_mut() else {
                    continue;
                };
                let region = memfd::create_and_populate(
                    &self.memory,
                    &format!("forkd-source-mem-{}", opts.netns_offset + i + 1),
                    use_hugepages,
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
        let mut handles: Vec<(usize, u32, thread::JoinHandle<Result<()>>)> = Vec::new();
        for (i, slot) in children.iter().enumerate() {
            let Some(c) = slot else {
                continue;
            };
            let sock = c.sock.clone();
            let pid = c.pid;
            let body = match &c.memfd {
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
            };
            handles.push((
                i,
                pid,
                thread::spawn(move || -> Result<()> {
                    api_call(&sock, "PUT", "/snapshot/load", &body)
                }),
            ));
        }
        for (i, pid, h) in handles {
            match h.join() {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    failures.push(RestoreFailure {
                        child_index: i + 1,
                        phase: RestorePhase::Restore,
                        pid: Some(pid),
                        error: format!("{e:#}"),
                    });
                    // Dropping the child kills its firecracker process.
                    children[i] = None;
                }
                // A panicked thread is an unrecoverable child failure —
                // collect it as a restore failure rather than panicking
                // the caller, preserving the partial-failure contract.
                Err(panic) => {
                    failures.push(RestoreFailure {
                        child_index: i + 1,
                        phase: RestorePhase::Restore,
                        pid: Some(pid),
                        error: format!("restore thread panicked: {panic:?}"),
                    });
                    children[i] = None;
                }
            }
        }
        let restore_ms = restore_start.elapsed().as_millis();

        // Phase 2.5: best-effort guest clock synchronization. After
        // restore, each child's guest clock reflects the snapshot's
        // creation time, not the host's current wall time. This causes
        // TLS certificate validation failures, incorrect timestamps,
        // and token expiry issues inside the guest.
        //
        // We poll the guest agent (forkd-agent.py on port 8888) until
        // it responds, then exec `date -s @<host_unix_ts>` to set the
        // guest clock to the host's current wall time. This is
        // best-effort and non-fatal: if the agent doesn't come up
        // within the timeout, or `date` is unavailable, the sandbox
        // remains usable with a stale clock.
        //
        // This complements the `clocksource=kvm-clock` boot arg
        // (PR #289), which is x86-only and forward-only (requires
        // re-baking snapshots). This approach works on any
        // architecture, any Linux kernel, and any existing snapshot.
        // See issue #288 for the full options analysis.
        let (clock_sync_ms, clock_sync_outcomes) = if opts.sync_guest_clocks {
            // `children` is `Vec<Option<Vm>>` (slots that failed spawn/
            // socket-wait are `None`). Clock sync only applies to the
            // successfully-restored children; the `child_index` used in
            // outcomes is the 1-based within-batch slot index so callers
            // can map outcomes back to resource slots.
            //
            // `sync_guest_clocks` only reads `child.netns` (cloned into the
            // sync thread) so it does not need to own the `Vm`. Pass the
            // live children by reference, preserving each child's ORIGINAL
            // 1-based slot (review #302: flattening would renumber a
            // survivor past a failed hole). `Vm` is not `Clone` (it owns a
            // Child process).
            let live: Vec<(usize, &Vm)> = live_children_with_indices(&children);
            sync_guest_clocks(&live)
        } else {
            (0, Vec::new())
        };

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
            for c in children.iter().flatten() {
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
            children: children
                .into_iter()
                .enumerate()
                .filter_map(|(i, slot)| {
                    slot.map(|vm| ForkChild {
                        child_index: i + 1,
                        vm,
                    })
                })
                .collect(),
            failures,
            spawn_ms,
            restore_ms,
            prewarm_ms,
            clock_sync_ms,
            clock_sync_outcomes,
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

    /// RAII guard that removes a directory on drop — even on panic.
    /// Used by integration tests to avoid leaking work_dir on failure.
    struct WorkDirGuard(std::path::PathBuf);
    impl Drop for WorkDirGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn kernel_supports_kvm_clock_realtime_parses_versions() {
        assert!(!kernel_supports_kvm_clock_realtime("5.10.0-foo"));
        assert!(!kernel_supports_kvm_clock_realtime("5.15.0-foo"));
        assert!(kernel_supports_kvm_clock_realtime("5.16.0-foo"));
        assert!(kernel_supports_kvm_clock_realtime("5.16.1-foo"));
        assert!(kernel_supports_kvm_clock_realtime("6.1.0-foo"));
        assert!(kernel_supports_kvm_clock_realtime("6.1.141"));
        assert!(!kernel_supports_kvm_clock_realtime("5.14.0-foo"));
        assert!(!kernel_supports_kvm_clock_realtime("garbage"));
        assert!(!kernel_supports_kvm_clock_realtime(""));
    }

    // #261: a deadline-bounded accept must give up instead of hanging
    // forever when no peer ever connects.
    #[test]
    #[cfg(target_os = "linux")]
    fn accept_with_deadline_times_out_when_nobody_connects() {
        use std::os::unix::net::UnixListener;
        let sock = std::env::temp_dir().join(format!("forkd-accept-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();
        listener.set_nonblocking(true).unwrap();
        let start = Instant::now();
        let r = accept_with_deadline(&listener, start + Duration::from_millis(60));
        assert!(r.is_err(), "should time out, got {r:?}");
        assert!(start.elapsed() < Duration::from_secs(2), "must not hang");
        let _ = std::fs::remove_file(&sock);
    }

    #[test]
    fn wait_for_sock_returns_immediately_when_path_exists() {
        let sock = std::env::temp_dir().join(format!(
            "forkd-wait-for-sock-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::write(&sock, b"ready").unwrap();
        wait_for_sock(&sock, Duration::from_secs(10)).unwrap();
        let _ = std::fs::remove_file(&sock);
    }

    #[test]
    fn boot_config_quickstart_has_sane_defaults() {
        let cfg = BootConfig::quickstart("/tmp/k".into(), "/tmp/r".into(), "/tmp/w".into());
        assert_eq!(cfg.vcpu_count, 2);
        assert_eq!(cfg.mem_size_mib, 512);
        assert!(cfg.boot_args.contains("console=ttyS0"));
        assert!(cfg.boot_args.contains("root=/dev/vda"));
        assert!(cfg.boot_args.contains(" ro"));
        // Forward-prep for #218: random.trust_cpu=on resolves
        // `getrandom(2)` blocking once the guest kernel is rebuilt
        // with Linux 4.19+. Quietly ignored on the shipped 4.14
        // image, but if a future commit ever drops the option the
        // post-kernel-rebuild fix silently regresses — defend it.
        assert!(
            cfg.boot_args.contains("random.trust_cpu=on"),
            "quickstart boot_args must include random.trust_cpu=on (see #218): `{}`",
            cfg.boot_args
        );
        assert!(cfg.rootfs_read_only);
        // clocksource=kvm-clock prevents the guest from switching to TSC,
        // which is stale after snapshot restore and breaks TLS validation.
        // Assert both presence AND that it is the last clocksource= token
        // (a later clocksource=tsc would silently override it).
        assert!(
            cfg.boot_args.contains("clocksource=kvm-clock"),
            "quickstart boot_args must include clocksource=kvm-clock: `{}`",
            cfg.boot_args
        );
        let after = cfg
            .boot_args
            .split("clocksource=kvm-clock")
            .nth(1)
            .unwrap_or("");
        assert!(
            !after.contains("clocksource="),
            "quickstart boot_args has a clocksource= token after kvm-clock \
             which would override it: `{}`",
            cfg.boot_args
        );
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
        assert!(
            cfg.boot_args.contains("clocksource=kvm-clock"),
            "ext4_rw boot_args must include clocksource=kvm-clock: `{}`",
            cfg.boot_args
        );
        let after = cfg
            .boot_args
            .split("clocksource=kvm-clock")
            .nth(1)
            .unwrap_or("");
        assert!(
            !after.contains("clocksource="),
            "ext4_rw boot_args has a clocksource= token after kvm-clock \
             which would override it: `{}`",
            cfg.boot_args
        );
    }

    /// The overlay config is what keeps a restored snapshot's rootfs
    /// byte-identical: the drive must be opened read-only on the host AND
    /// mounted `ro` in the guest, and the guest must run our init so it can
    /// provide the writable layer. Getting this wrong is silent corruption
    /// rather than a failing test in production, so assert all three.
    #[test]
    fn boot_config_ext4_overlay_is_read_only_with_guest_writable_layer() {
        let cfg = BootConfig::ext4_overlay("/tmp/k".into(), "/tmp/r.ext4".into(), "/tmp/w".into());
        assert!(
            cfg.rootfs_read_only,
            "the host must open the shared ext4 read-only"
        );
        assert!(
            cfg.boot_args.contains(" root=/dev/vda ro "),
            "the guest kernel must mount the shared ext4 read-only, got: {}",
            cfg.boot_args
        );
        assert!(
            !cfg.boot_args.contains(" rw "),
            "ext4_overlay must not request a writable root: `{}`",
            cfg.boot_args
        );
        assert!(
            cfg.boot_args.contains("init=/forkd-init.sh"),
            "the guest writable layer lives in /forkd-init.sh: `{}`",
            cfg.boot_args
        );
        // Same rationale as quickstart(); a later token would override it.
        assert!(cfg.boot_args.contains("clocksource=kvm-clock"));
        let after = cfg
            .boot_args
            .split("clocksource=kvm-clock")
            .nth(1)
            .unwrap_or("");
        assert!(
            !after.contains("clocksource="),
            "duplicate clocksource token"
        );
    }

    /// The writable layer is capped, and the cap is reachable from the host
    /// because a job that outgrows it must fail its own write rather than
    /// OOM the guest.
    #[test]
    fn with_rw_size_appends_the_cmdline_hint() {
        let cfg = BootConfig::ext4_overlay("/tmp/k".into(), "/tmp/r.ext4".into(), "/tmp/w".into())
            .with_rw_size("8g");
        assert!(
            cfg.boot_args.contains("forkd.rw_size=8g"),
            "init reads the cap from the cmdline: `{}`",
            cfg.boot_args
        );
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
    fn fork_opts_sync_guest_clocks_defaults_disabled() {
        // Clock sync is opt-in. The default must keep it disabled so
        // custom/no-agent snapshots don't add the ~10s agent-poll
        // latency to every restore — the "no added latency by default"
        // guarantee for images without forkd-agent.
        assert!(
            !ForkOpts::default().sync_guest_clocks,
            "sync_guest_clocks must default to false (opt-in)"
        );
    }

    #[test]
    fn restore_many_with_rejects_multi_child_shared_ip() {
        // n>1 with per_child_netns=false means all children share the
        // guest agent address 10.42.0.2:8888 and MAC, so per-child
        // addressing is impossible. restore_many_with must reject this
        // BEFORE spawning any Firecracker process or touching the
        // filesystem. This exercises the multi-child address/mode guard
        // the reviewer asked to cover.
        let snap = Snapshot {
            vmstate: "/nonexistent/vmstate.bin".into(),
            memory: "/nonexistent/memory.bin".into(),
            volumes: Vec::new(),
            parent_tag: None,
            parent_content_hash: None,
            rootfs: None,
        };
        let opts = ForkOpts {
            n: 2,
            per_child_netns: false,
            ..ForkOpts::default()
        };
        let err = snap
            .restore_many_with(opts, Path::new("/nonexistent/forkd-test-workdir"))
            .expect_err("n>1 with per_child_netns=false must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("per_child_netns=false"),
            "error should name the per_child_netns=false mode, got: {msg}"
        );
        assert!(
            msg.contains("n>1"),
            "error should name the multi-child condition, got: {msg}"
        );
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
            rootfs: None,
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
            rootfs: None,
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

    // -----------------------------------------------------------------
    // Integration test: boot a real Firecracker VM and verify the
    // guest clocksource is kvm-clock (not TSC).
    //
    // This test verifies the *effect* of `clocksource=kvm-clock` in
    // boot_args, not just that the string is present. It boots an
    // actual firecracker VM with a real kernel and rootfs, waits for
    // the guest agent, and reads
    //   /sys/devices/system/clocksource/clocksource0/current_clocksource
    // inside the VM.
    //
    // Requirements:
    //   - Linux host with Firecracker installed
    //   - forkd-tap0 tap device configured (run `scripts/host-tap.sh`)
    //   - FORKD_TEST_KERNEL env var pointing to a vmlinux (default:
    //     /var/lib/forkd/kernels/vmlinux)
    //   - FORKD_TEST_ROOTFS env var pointing to an ext4 rootfs image
    //     that includes python3 (for forkd-agent.py / forkd-init.sh)
    //
    // Run with:
    //   FORKD_TEST_ROOTFS=/var/cache/forkd/dev-base.ext4 \
    //     cargo test -p forkd-vmm --lib boot_vm_uses_kvm_clock -- --ignored
    // -----------------------------------------------------------------
    #[cfg(target_os = "linux")]
    #[ignore = "requires firecracker, kernel, rootfs, and tap device"]
    #[test]
    fn boot_vm_uses_kvm_clock() {
        let kernel = std::env::var("FORKD_TEST_KERNEL")
            .unwrap_or_else(|_| "/var/lib/forkd/kernels/vmlinux".to_string());
        let rootfs = std::env::var("FORKD_TEST_ROOTFS")
            .expect("FORKD_TEST_ROOTFS must point to an ext4 rootfs image");

        // Pre-flight: verify kernel and rootfs exist for actionable errors.
        assert!(
            std::path::Path::new(&kernel).exists(),
            "FORKD_TEST_KERNEL not found at {kernel}"
        );
        assert!(
            std::path::Path::new(&rootfs).exists(),
            "FORKD_TEST_ROOTFS not found at {rootfs}"
        );

        let work_dir =
            std::env::temp_dir().join(format!("forkd-clocksource-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&work_dir);
        // RAII guard: removes work_dir on drop, even on panic.
        let _work_dir_guard = WorkDirGuard(work_dir.clone());

        let addr = "10.42.0.2:8888";

        // Pre-flight: ensure no stale VM is already using 10.42.0.2.
        assert!(
            ping_at(addr).is_err(),
            "10.42.0.2:8888 is already reachable — a stale VM may occupy \
             the address; kill it before running this test"
        );

        let cfg = BootConfig::ext4_rw(kernel.into(), rootfs.into(), work_dir.clone())
            .with_network(NetworkConfig::default_tap("forkd-tap0"));

        // Boot the VM. Vm::boot blocks until Firecracker accepts
        // InstanceStart but does NOT wait for guest userspace.
        let vm = Vm::boot(&cfg).expect("boot VM");

        // Poll the guest agent until it responds. The guest needs
        // time to run forkd-init.sh and start the agent listener.
        // Worst case is ~120s if the tap blocks SYN (2s connect_timeout
        // × 60 iterations), but typically connection is refused and
        // the loop completes in ~30s.
        let mut connected = false;
        for _ in 0..60 {
            std::thread::sleep(std::time::Duration::from_millis(500));
            if ping_at(addr).is_ok() {
                connected = true;
                break;
            }
        }
        assert!(
            connected,
            "guest agent at {addr} did not respond within 30s; \
             check console log at {}/fc.console",
            work_dir.display()
        );

        // Read the active clocksource from inside the VM.
        let resp = exec_at(
            addr,
            vec![
                "cat".into(),
                "/sys/devices/system/clocksource/clocksource0/current_clocksource".into(),
            ],
            std::time::Duration::from_secs(10),
        )
        .expect("exec clocksource check");

        assert_eq!(
            resp.exit_code, 0,
            "cat clocksource failed with exit {}:\n{}",
            resp.exit_code, resp.stderr
        );
        let clocksource = resp.stdout.trim();
        assert_eq!(
            clocksource, "kvm-clock",
            "expected clocksource=kvm-clock but guest reports '{clocksource}' \
             — the kernel may have switched to TSC despite the boot parameter; \
             check if the guest kernel supports clocksource=kvm-clock"
        );

        // Verify the symptom: guest wall clock should match the host
        // (within a tolerance). This catches the actual bug — stale
        // TSC after snapshot restore makes the guest clock wrong by
        // days/weeks. A fresh boot with kvm-clock should be within a
        // few seconds of the host.
        //
        // Note: this test boots a fresh VM, not a snapshot-restored
        // one. The snapshot-restore path (where stale TSC actually
        // manifests) is not directly tested here — that would require
        // creating a snapshot, restoring it, and re-checking. This
        // test verifies the *mechanism* (clocksource stays kvm-clock)
        // and the *symptom on fresh boot* (correct wall clock). A
        // follow-up restore-path test would close the remaining gap.
        let host_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("host time before epoch")
            .as_secs();
        let time_resp = exec_at(
            addr,
            vec!["date".into(), "-u".into(), "+%s".into()],
            std::time::Duration::from_secs(10),
        )
        .expect("exec date check");
        assert_eq!(
            time_resp.exit_code, 0,
            "date command failed with exit {}: {}",
            time_resp.exit_code, time_resp.stderr
        );
        let guest_time: u64 = time_resp
            .stdout
            .trim()
            .parse()
            .expect("guest date output is not a valid epoch");
        let drift = host_time.abs_diff(guest_time);
        assert!(
            drift <= 5,
            "guest wall clock drifts {drift}s from host (host={host_time}, guest={guest_time}) \
             — kvm-clock should keep the guest synced to the host; \
             large drift indicates the clocksource is not working as expected"
        );

        // Vm::drop kills firecracker and removes the socket.
        // WorkDirGuard drops work_dir.
        drop(vm);
    }

    /// Integration test: verify kvm-clock survives snapshot/restore on
    /// modern x86 kernels (≥ 5.16 with KVM_CLOCK_REALTIME).
    ///
    /// This test complements boot_vm_uses_kvm_clock (which tests fresh
    /// boot only). The snapshot-restore path is where stale TSC actually
    /// manifests: after restore, KVM_SET_CLOCK restores the saved clock
    /// state from the snapshot. On Linux ≥ 5.16, the KVM_CLOCK_REALTIME
    /// flag makes kvm-clock auto-correct to host wall time, so the boot
    /// arg alone is sufficient. On older kernels (< 5.16) or aarch64,
    /// the restored clock is stale — see PR #300 (guest-side date -s)
    /// for the complementary fix.
    ///
    /// This test verifies #289's contribution: the boot arg keeps
    /// kvm-clock as the clocksource through the snapshot/restore cycle,
    /// and on modern kernels the restored clock is correct without any
    /// guest-side intervention.
    #[test]
    #[ignore = "requires firecracker, kernel, rootfs, and tap device"]
    #[cfg(target_os = "linux")]
    fn kvm_clock_survives_snapshot_restore() {
        let kernel = std::env::var("FORKD_TEST_KERNEL")
            .unwrap_or_else(|_| "/var/lib/forkd/kernels/vmlinux".to_string());
        let rootfs = std::env::var("FORKD_TEST_ROOTFS")
            .expect("FORKD_TEST_ROOTFS must point to an ext4 rootfs image");

        assert!(
            std::path::Path::new(&kernel).exists(),
            "FORKD_TEST_KERNEL not found at {kernel}"
        );
        assert!(
            std::path::Path::new(&rootfs).exists(),
            "FORKD_TEST_ROOTFS not found at {rootfs}"
        );

        let boot_dir =
            std::env::temp_dir().join(format!("forkd-restore-test-boot-{}", std::process::id()));
        let snap_dir =
            std::env::temp_dir().join(format!("forkd-restore-test-snap-{}", std::process::id()));
        let restore_dir =
            std::env::temp_dir().join(format!("forkd-restore-test-restore-{}", std::process::id()));

        let _ = std::fs::remove_dir_all(&boot_dir);
        let _ = std::fs::remove_dir_all(&snap_dir);
        let _ = std::fs::remove_dir_all(&restore_dir);

        let _boot_guard = WorkDirGuard(boot_dir.clone());
        let _snap_guard = WorkDirGuard(snap_dir.clone());
        let _restore_guard = WorkDirGuard(restore_dir.clone());

        let addr = "10.42.0.2:8888";

        assert!(
            ping_at(addr).is_err(),
            "{addr} is already reachable — a stale VM may occupy the address; \
             kill it before running this test"
        );

        // --- Phase 1: Boot fresh VM and verify kvm-clock ---

        let cfg = BootConfig::ext4_rw(kernel.into(), rootfs.into(), boot_dir.clone())
            .with_network(NetworkConfig::default_tap("forkd-tap0"));

        let vm = Vm::boot(&cfg).expect("boot VM");

        let mut connected = false;
        for _ in 0..60 {
            std::thread::sleep(std::time::Duration::from_millis(500));
            if ping_at(addr).is_ok() {
                connected = true;
                break;
            }
        }
        assert!(
            connected,
            "guest agent at {addr} did not respond within 30s; \
             check console log at {}/fc.console",
            boot_dir.display()
        );

        // Verify clocksource on fresh boot
        let resp = exec_at(
            addr,
            vec![
                "cat".into(),
                "/sys/devices/system/clocksource/clocksource0/current_clocksource".into(),
            ],
            std::time::Duration::from_secs(10),
        )
        .expect("exec clocksource check");
        assert_eq!(resp.exit_code, 0, "cat clocksource failed: {}", resp.stderr);
        assert_eq!(
            resp.stdout.trim(),
            "kvm-clock",
            "expected clocksource=kvm-clock on fresh boot"
        );

        // --- Phase 2: Snapshot the VM ---

        vm.pause().expect("pause VM for snapshot");

        let vmstate_path = snap_dir.join("vmstate.bin");
        let memory_path = snap_dir.join("memory.bin");
        let snapshot = vm
            .snapshot_to(vmstate_path, memory_path, vec![])
            .expect("create snapshot");

        // Kill the original VM to free the tap device and guest agent
        // address. Vm::drop kills firecracker and removes the socket.
        // The tap device persists (pre-created by scripts/host-tap.sh).
        drop(vm);

        // Wait comfortably longer than the allowed drift so a guest
        // that simply resumes at the saved timestamp is caught. On
        // modern kernels (≥ 5.16) KVM_CLOCK_REALTIME makes kvm-clock
        // auto-correct on restore, so this wait doesn't affect
        // correctness. On older kernels (5.10/5.15) the restored clock
        // would be off by ~this amount, and the drift assertion will
        // fail — directing the operator to PR #300 for the complementary
        // guest-side fix. The wait must be comfortably larger than the
        // allowed drift (≤ 2 s) so a stale clock cannot pass by accident.
        let wait_secs: u64 = 20;
        std::thread::sleep(std::time::Duration::from_secs(wait_secs));

        // --- Phase 3: Restore from snapshot and verify kvm-clock ---

        assert!(
            ping_at(addr).is_err(),
            "{addr} is still reachable after killing the source VM"
        );

        let opts = ForkOpts {
            n: 1,
            per_child_netns: false,
            memory_limit_mib: None,
            netns_offset: 0,
            prewarm_scratch_dir: None,
            memory_backend: MemoryBackend::File,
            enable_diff_snapshots: false,
            socket_wait_timeout_secs: DEFAULT_SOCKET_WAIT_SECS,
            sync_guest_clocks: false,
        };

        let fork_result = snapshot
            .restore_many_with(opts, &restore_dir)
            .expect("restore from snapshot");

        assert_eq!(
            fork_result.children.len(),
            1,
            "expected 1 restored child, got {}",
            fork_result.children.len()
        );

        let restored_vm = fork_result.children.into_iter().next().unwrap().vm;

        // Wait for guest agent on the restored VM
        let mut connected = false;
        for _ in 0..60 {
            std::thread::sleep(std::time::Duration::from_millis(500));
            if ping_at(addr).is_ok() {
                connected = true;
                break;
            }
        }
        assert!(
            connected,
            "guest agent at {addr} did not respond after restore within 30s; \
             check console log at {}/child-1.console",
            restore_dir.display()
        );

        // Verify clocksource is still kvm-clock after restore
        let resp = exec_at(
            addr,
            vec![
                "cat".into(),
                "/sys/devices/system/clocksource/clocksource0/current_clocksource".into(),
            ],
            std::time::Duration::from_secs(10),
        )
        .expect("exec clocksource check after restore");
        assert_eq!(resp.exit_code, 0, "cat clocksource failed: {}", resp.stderr);
        assert_eq!(
            resp.stdout.trim(),
            "kvm-clock",
            "clocksource changed after snapshot/restore — \
             expected kvm-clock to persist"
        );

        // Verify wall clock is correct after restore. On modern x86
        // (≥ 5.16), KVM_CLOCK_REALTIME makes kvm-clock auto-correct to
        // host time on restore. On older kernels (5.10/5.15),
        // KVM_CLOCK_REALTIME is unavailable and the restored kvm-clock
        // stays stale — that gap is addressed by PR #300 (guest-side
        // date -s after restore). We detect the host kernel version
        // here so the behavior is explicit rather than silent.
        let host_kernel = host_kernel_release();
        let host_time = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .expect("host time before epoch")
            .as_secs();
        let time_resp = exec_at(
            addr,
            vec!["date".into(), "-u".into(), "+%s".into()],
            std::time::Duration::from_secs(10),
        )
        .expect("exec date check after restore");
        assert_eq!(time_resp.exit_code, 0, "date failed: {}", time_resp.stderr);
        let guest_time: u64 = time_resp.stdout.trim().parse().expect("parse guest time");
        let drift = host_time.abs_diff(guest_time);

        // The wait (20 s) is comfortably larger than the allowed drift
        // (2 s), so a guest that resumes at the saved timestamp
        // (~20 s in the past) cannot pass by accident.
        let max_drift: u64 = 2;
        let modern = host_kernel
            .as_ref()
            .map(|k| kernel_supports_kvm_clock_realtime(k))
            .unwrap_or(true); // assume modern if we can't detect
        if modern {
            assert!(
                drift <= max_drift,
                "guest wall clock drifts {drift}s from host after restore \
                 (host={host_time}, guest={guest_time}, kernel={host_kernel:?}) — \
                 KVM_CLOCK_REALTIME (kernel ≥ 5.16) should auto-correct kvm-clock \
                 on restore; drift > {max_drift}s means the correction is not working"
            );
        } else {
            // On older kernels (5.10/5.15) KVM_CLOCK_REALTIME is
            // unavailable. The boot-arg fix alone is insufficient —
            // the guest clock stays stale after restore. This is the
            // documented gap that PR #300 (guest-side date -s) fills.
            // We still assert so the limitation is visible, but the
            // message points to the complementary fix.
            assert!(
                drift <= max_drift,
                "guest wall clock drifts {drift}s from host after restore \
                 (host={host_time}, guest={guest_time}, kernel={host_kernel:?}) — \
                 this kernel does not support KVM_CLOCK_REALTIME (needs ≥ 5.16); \
                 the boot-arg fix alone cannot correct the clock on restore. \
                 PR #300 (guest-side date -s after restore) is required for \
                 this kernel range"
            );
        }

        // Clean up
        drop(restored_vm);
    }

    // Verify that the date command format used by sync_guest_clocks
    // produces a valid `date -s @<unix_ts>` invocation. We can't test
    // the full clock sync path (requires real firecracker VMs), but we
    // can verify the command string is well-formed.
    #[test]
    fn clock_sync_date_command_format() {
        let ts: u64 = 1723430400; // 2024-08-12T00:00:00Z
        let cmd = format!("date -s @{ts}");
        assert!(cmd.starts_with("date -s @"));
        assert!(cmd.ends_with('0')); // ends with a digit
                                     // The timestamp must be parseable as a valid Unix epoch value.
        let parsed: u64 = cmd.strip_prefix("date -s @").unwrap().parse().unwrap();
        assert_eq!(parsed, ts);
    }

    // Verify ForkResult includes the clock_sync_ms field. This is a
    // compile-time check — if the struct ever loses the field, this
    // test won't compile.
    #[test]
    fn fork_result_has_clock_sync_ms_field() {
        let fr = ForkResult {
            children: vec![],
            failures: vec![],
            spawn_ms: 0,
            restore_ms: 0,
            prewarm_ms: 0,
            clock_sync_ms: 0,
            clock_sync_outcomes: Vec::new(),
        };
        assert_eq!(fr.clock_sync_ms, 0);
    }

    // Verify ClockSyncOutcome enum discriminants and field access.
    // This is a compile-time + runtime check that the enum is
    // constructible and matchable as expected by callers.
    #[test]
    fn clock_sync_outcome_construction_and_matching() {
        let synced = ClockSyncOutcome::Synced { child_index: 0 };
        let failed = ClockSyncOutcome::Failed {
            child_index: 1,
            error: "agent timeout".to_string(),
        };

        assert!(matches!(
            synced,
            ClockSyncOutcome::Synced { child_index: 0 }
        ));
        assert!(matches!(
            &failed,
            ClockSyncOutcome::Failed { child_index: 1, .. }
        ));

        // Verify the error field is accessible.
        if let ClockSyncOutcome::Failed { error, .. } = &failed {
            assert_eq!(error, "agent timeout");
        }

        // Verify a Vec of outcomes can be filtered for failures
        // (the pattern callers use to decide fail-closed).
        let outcomes = [synced, failed];
        let failures: Vec<String> = outcomes
            .iter()
            .filter_map(|o| match o {
                ClockSyncOutcome::Failed { child_index, error } => {
                    Some(format!("child {child_index}: {error}"))
                }
                _ => None,
            })
            .collect();
        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains("agent timeout"));
    }

    /// Regression (review #302): the index-preserving mapping that feeds
    /// `sync_guest_clocks` must NOT renumber a survivor past a hole.
    ///
    /// `restore_many_with` keeps per-slot results as `Vec<Option<Vm>>`
    /// (failed spawn/socket-wait slots are `None`). A naive
    /// `children.iter().flatten()` re-indexes the survivors, so with
    /// `[None, Some(a), None, Some(b)]` the flattened slice is `[a, b]`
    /// and `enumerate()` would report them as slots 0/1 instead of the
    /// original 2/4 — breaking the resource correlation that
    /// `ForkChild`/`RestoreFailure`/`ClockSyncOutcome` all promise via
    /// their 1-based within-batch index. Use cheap `u32` payloads so this
    /// is a portable, KVM-free unit test.
    #[test]
    fn live_children_with_indices_preserves_survivor_slot_across_hole() {
        // Vec positions 0 and 2 are None (failed), positions 1 and 3 are
        // live. The survivors must retain their ORIGINAL 1-based slots
        // (2 and 4), not be renumbered to 0/1 by flattening.
        let children: Vec<Option<u32>> = vec![None, Some(10), None, Some(30)];
        let live = live_children_with_indices(&children);

        assert_eq!(
            live.len(),
            2,
            "two live children expected from 2 holes + 2 survivors"
        );
        assert_eq!(live[0], (2, &10), "first survivor keeps slot 2");
        assert_eq!(live[1], (4, &30), "second survivor keeps slot 4");

        // And the adjacency case: no holes → indices map 1..=n in order.
        let all: Vec<Option<u32>> = vec![Some(1), Some(2), Some(3)];
        let live_all = live_children_with_indices(&all);
        assert_eq!(
            live_all.iter().map(|(i, _)| *i).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );

        // All failures → empty result.
        let none: Vec<Option<u32>> = vec![None, None];
        assert!(live_children_with_indices(&none).is_empty());

        // Trailing hole: the live child keeps its slot even when the tail
        // is empty.
        let trailing: Vec<Option<u32>> = vec![Some(5), None];
        assert_eq!(live_children_with_indices(&trailing), vec![(1, &5)]);

        // First-live-then-holes: a survivor followed only by holes keeps
        // its (first) slot.
        let first_live: Vec<Option<u32>> = vec![Some(9), None, None];
        assert_eq!(live_children_with_indices(&first_live), vec![(1, &9)]);

        // Empty input → empty result.
        let empty: Vec<Option<u32>> = Vec::new();
        assert!(live_children_with_indices(&empty).is_empty());
    }

    /// Integration test: sync_guest_clocks corrects a deliberately-wrong
    /// guest clock. This isolates the scripting solution — even if the
    /// kernel's KVM_CLOCK_REALTIME already corrected the clock after
    /// restore, the test forces a wrong clock and verifies the sync
    /// function fixes it. This tests the *mechanism* (date -s via guest
    /// agent exec), not the *environment* (kernel clock behavior).
    ///
    /// The mock clock underneath is fine: we deliberately set the guest
    /// clock to 2001-09-09, then run sync_guest_clocks and verify the
    /// guest clock is corrected to the host's wall time.
    #[test]
    #[ignore = "requires Linux + KVM + firecracker + rootfs image"]
    #[cfg(target_os = "linux")]
    fn sync_guest_clocks_corrects_wrong_clock() {
        let kernel = std::env::var("FORKD_TEST_KERNEL")
            .unwrap_or_else(|_| "/var/lib/forkd/kernels/vmlinux".to_string());
        let rootfs = std::env::var("FORKD_TEST_ROOTFS")
            .expect("FORKD_TEST_ROOTFS must point to an ext4 rootfs image");

        // Pre-flight: verify kernel and rootfs exist.
        assert!(
            std::path::Path::new(&kernel).exists(),
            "FORKD_TEST_KERNEL not found at {kernel}"
        );
        assert!(
            std::path::Path::new(&rootfs).exists(),
            "FORKD_TEST_ROOTFS not found at {rootfs}"
        );

        let work_dir =
            std::env::temp_dir().join(format!("forkd-clocksync-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&work_dir);
        let _work_dir_guard = WorkDirGuard(work_dir.clone());

        let addr = GUEST_AGENT_ADDR.to_string();

        // Pre-flight: ensure no stale VM is already using the address.
        assert!(
            ping_at(&addr).is_err(),
            "{addr} is already reachable — a stale VM may occupy the address; \
             kill it before running this test"
        );

        let cfg = BootConfig::ext4_rw(kernel.into(), rootfs.into(), work_dir.clone())
            .with_network(NetworkConfig::default_tap("forkd-tap0"));

        let vm = Vm::boot(&cfg).expect("boot VM");

        // Poll the guest agent until it responds.
        let mut connected = false;
        for _ in 0..60 {
            std::thread::sleep(Duration::from_millis(500));
            if ping_at(&addr).is_ok() {
                connected = true;
                break;
            }
        }
        assert!(
            connected,
            "guest agent at {addr} did not respond within 30s; \
             check console log at {}/fc.console",
            work_dir.display()
        );

        // Step 1: Deliberately set the guest clock to an old time.
        // 1000000000 = 2001-09-09T01:46:40Z — far enough in the past to
        // be unambiguous with the host's current time.
        let old_ts = 1_000_000_000u64;
        let set_resp = exec_at(
            &addr,
            vec!["sh".into(), "-c".into(), format!("date -s @{old_ts}")],
            Duration::from_secs(10),
        )
        .expect("exec date -s to set wrong clock");
        assert_eq!(
            set_resp.exit_code, 0,
            "date -s @{old_ts} failed with exit {}: {}",
            set_resp.exit_code, set_resp.stderr
        );

        // Step 2: Verify the guest clock IS wrong.
        let wrong_resp = exec_at(
            &addr,
            vec!["date".into(), "-u".into(), "+%s".into()],
            Duration::from_secs(10),
        )
        .expect("exec date to read wrong clock");
        let wrong_time: u64 = wrong_resp.stdout.trim().parse().expect("parse guest time");
        assert!(
            wrong_time.abs_diff(old_ts) <= 2,
            "guest clock should be ~{old_ts} after date -s, got {wrong_time}"
        );

        // Step 3: Run sync_guest_clocks — this is what #300 adds after
        // restore. The function captures host wall time and execs
        // `sh -c "date -s @<host_ts>"` in each child VM. The single
        // child is passed as slot 1 (1-based within-batch), so its
        // outcome must report child_index = 1.
        let (elapsed_ms, outcomes) = sync_guest_clocks(&[(1, &vm)]);
        assert!(
            elapsed_ms < 30_000,
            "sync_guest_clocks took {elapsed_ms}ms, expected < 30s"
        );
        // Verify per-child outcome: exactly one Synced for the single child.
        assert_eq!(
            outcomes.len(),
            1,
            "expected 1 clock sync outcome, got {}",
            outcomes.len()
        );
        assert!(
            matches!(&outcomes[0], ClockSyncOutcome::Synced { child_index: 1 }),
            "expected Synced{{child_index: 1}}, got {:?}",
            outcomes[0]
        );

        // Step 4: Verify the guest clock is now correct (close to host
        // time). This is the key assertion — the sync function should
        // have overwritten the mock old clock with the host's wall time.
        let host_time = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("host time before epoch")
            .as_secs();

        let fixed_resp = exec_at(
            &addr,
            vec!["date".into(), "-u".into(), "+%s".into()],
            Duration::from_secs(10),
        )
        .expect("exec date to read fixed clock");
        let fixed_time: u64 = fixed_resp
            .stdout
            .trim()
            .parse()
            .expect("parse fixed guest time");

        let drift = host_time.abs_diff(fixed_time);
        assert!(
            drift <= 5,
            "guest clock drifts {drift}s from host after sync_guest_clocks \
             (host={host_time}, guest={fixed_time}) — the sync function should \
             have corrected the clock to host wall time"
        );

        // The corrected time must NOT be the old mock time.
        assert!(
            fixed_time.abs_diff(old_ts) > 60,
            "guest clock ({fixed_time}) is still ~{old_ts} (the mock old time) \
             — sync_guest_clocks did not correct the clock"
        );

        // Vm::drop kills firecracker and removes the socket.
        // WorkDirGuard drops work_dir.
        drop(vm);
    }

    /// Deterministic unit test: `sync_one_child_clock` captures a FRESH
    /// timestamp AFTER the readiness probe succeeds, not before polling.
    ///
    /// This is the regression for the delayed-agent-readiness finding: the
    /// previous `sync_guest_clocks_delayed_agent_readiness` integration test
    /// was non-deterministic — it called sync immediately and only checked
    /// the final drift plus an upper elapsed bound, so it never *guaranteed*
    /// that agent readiness was actually delayed beyond the accepted 3-second
    /// drift. If the agent came up quickly, the old pre-poll-timestamp
    /// implementation still passed.
    ///
    /// This test injects a readiness probe that **deterministically** fails
    /// for 4 seconds (> the 3s accepted drift) then succeeds, and a `now`
    /// source that records when it was called relative to the probe. It then
    /// asserts:
    ///   1. the induced poll delay (4s) exceeds the accepted clock drift (3s),
    ///   2. the timestamp passed to `set_clock` was captured AFTER ping
    ///      succeeded (i.e. fresh), so a pre-poll-timestamp implementation
    ///      would fail this assertion by ~4s,
    ///   3. `set_clock` is actually invoked with that fresh timestamp.
    ///
    /// No VM, guest agent, or KVM is required — this runs in the unit-test
    /// suite on every platform. The end-to-end mechanism (real agent +
    /// `date -s`) is still covered by the `#[ignore]` integration test
    /// `sync_guest_clocks_corrects_wrong_clock`.
    #[test]
    fn sync_one_child_clock_timestamp_fresh_after_delayed_ping() {
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
        use std::sync::Arc;

        // The accepted clock drift the integration test used was 3 seconds.
        // The induced poll delay MUST exceed it, otherwise the test cannot
        // distinguish fresh-from-stale timestamp placement.
        const ACCEPTED_DRIFT_SECS: u64 = 3;
        const INDUCED_DELAY_SECS: u64 = ACCEPTED_DRIFT_SECS + 1; // 4s
        let induced_delay = Duration::from_secs(INDUCED_DELAY_SECS);

        // Shared mutable state for the injected probes.
        let probe_start = Instant::now();
        let probe_ready_at = Arc::new(AtomicU64::new(0)); // wall-time secs when ping first Ok'd
        let now_called_at = Arc::new(AtomicU64::new(0)); // wall-time secs when `now` was called
        let set_clock_called = Arc::new(AtomicBool::new(false));
        let set_clock_ts = Arc::new(AtomicU64::new(0)); // timestamp passed to set_clock

        // Readiness probe: fails for `induced_delay`, then succeeds.
        // Records the wall time of the first success.
        let probe_ready_at_cl = probe_ready_at.clone();
        let ping = move || -> Result<()> {
            if probe_start.elapsed() >= induced_delay {
                probe_ready_at_cl.store(
                    SystemTime::now()
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                    Ordering::SeqCst,
                );
                Ok(())
            } else {
                bail!("agent not ready")
            }
        };

        // Timestamp source: returns wall time and records when it was called.
        let now_called_at_cl = now_called_at.clone();
        let now = || {
            let secs = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            now_called_at_cl.store(secs, Ordering::SeqCst);
            secs
        };

        // Clock-setter: records the timestamp it was invoked with.
        let set_clock_called_cl = set_clock_called.clone();
        let set_clock_ts_cl = set_clock_ts.clone();
        let set_clock = move |ts: u64| {
            set_clock_called_cl.store(true, Ordering::SeqCst);
            set_clock_ts_cl.store(ts, Ordering::SeqCst);
            Ok(())
        };

        let poll_start = Instant::now();
        sync_one_child_clock(ping, now, set_clock, CLOCK_SYNC_AGENT_TIMEOUT)
            .expect("sync_one_child_clock should succeed once the probe is ready");
        let poll_elapsed = poll_start.elapsed();

        // (1) The induced poll delay must exceed the accepted drift — this is
        // what makes the test deterministic. If the probe became ready before
        // the accepted drift, the test could not distinguish fresh from stale.
        assert!(
            poll_elapsed >= induced_delay,
            "poll elapsed {:?} is less than the induced delay {:?} — \
             the probe became ready too early, so the freshness assertion \
             below would not catch a pre-poll timestamp",
            poll_elapsed,
            induced_delay
        );
        // Compile-time invariant: the induced delay MUST exceed the accepted
        // drift, otherwise the freshness assertion below could not distinguish
        // a fresh (post-ping) timestamp from a stale (pre-poll) one.
        const _: () = {
            assert!(INDUCED_DELAY_SECS > ACCEPTED_DRIFT_SECS);
        };

        // (2) The timestamp was captured AFTER ping succeeded. The old
        // pre-poll implementation captured `now()` before the loop, so
        // `now_called_at` would be ~`INDUCED_DELAY_SECS` earlier than
        // `probe_ready_at`. A fresh (post-ping) implementation captures
        // `now()` after, so `now_called_at >= probe_ready_at`.
        let ready_at = probe_ready_at.load(Ordering::SeqCst);
        let now_at = now_called_at.load(Ordering::SeqCst);
        assert!(
            ready_at > 0,
            "readiness probe never succeeded — ping did not record a \
             ready timestamp"
        );
        assert!(
            now_at >= ready_at,
            "timestamp was captured at {now_at}, BEFORE ping succeeded at \
             {ready_at} (stale by ~{}s) — `now()` must be called AFTER the \
             readiness probe succeeds, not before polling",
            ready_at.saturating_sub(now_at)
        );

        // The freshness gap (now - ready) must be well under the accepted
        // drift. With a fresh post-ping timestamp this is sub-second; with
        // the old pre-poll timestamp it would be ~INDUCED_DELAY_SECS.
        let freshness_gap = now_at.saturating_sub(ready_at);
        assert!(
            freshness_gap <= ACCEPTED_DRIFT_SECS,
            "timestamp freshness gap is {freshness_gap}s (>= accepted drift \
             {ACCEPTED_DRIFT_SECS}s) — the timestamp was captured too long \
             before set_clock, reproducing the stale-clock bug"
        );

        // (3) set_clock was actually invoked with the fresh timestamp.
        assert!(
            set_clock_called.load(Ordering::SeqCst),
            "set_clock was never called"
        );
        let recorded_ts = set_clock_ts.load(Ordering::SeqCst);
        assert_eq!(
            recorded_ts, now_at,
            "set_clock was invoked with ts={recorded_ts}, but `now()` \
             returned {now_at} — the timestamp passed to the clock-setter \
             must be the one captured after ping"
        );
    }

    /// Unit test: `sync_one_child_clock` bails with a timeout error when the
    /// readiness probe never succeeds within the timeout. Covers the timeout
    /// path flagged by the testing/reliability/adversarial reviewers — without
    /// this, only the `#[ignore]` integration tests exercised it, and the
    /// 10-second production timeout made a dedicated unit test impractical.
    ///
    /// The timeout is injected (500ms) so the test runs fast and
    /// deterministically. It also asserts that `now()` and `set_clock` are
    /// never called on the timeout path (no point capturing a timestamp or
    /// setting the clock if the agent never came up).
    #[test]
    fn sync_one_child_clock_times_out_when_probe_never_succeeds() {
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
        use std::sync::Arc;

        let now_called = Arc::new(AtomicBool::new(false));
        let set_clock_called = Arc::new(AtomicBool::new(false));
        let ping_calls = Arc::new(AtomicU64::new(0));

        // Probe that always fails and counts how many times it was polled.
        let ping_calls_cl = ping_calls.clone();
        let ping = move || -> Result<()> {
            ping_calls_cl.fetch_add(1, Ordering::SeqCst);
            bail!("agent never ready")
        };

        let now_called_cl = now_called.clone();
        let now = || {
            now_called_cl.store(true, Ordering::SeqCst);
            0
        };

        let set_clock_called_cl = set_clock_called.clone();
        let set_clock = move |_ts: u64| {
            set_clock_called_cl.store(true, Ordering::SeqCst);
            Ok(())
        };

        // Inject a short timeout so the test doesn't wait 10s.
        let timeout = Duration::from_millis(500);
        let start = Instant::now();
        let result = sync_one_child_clock(ping, now, set_clock, timeout);
        let elapsed = start.elapsed();

        // The call must error with a timeout message.
        let err = result.expect_err("expected a timeout error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("did not respond within"),
            "timeout error should mention the timeout, got: {msg}"
        );
        assert!(
            msg.contains("0s") || msg.contains("500ms"),
            "timeout error should reflect the injected sub-second timeout, \
             got: {msg}"
        );

        // It must have polled at least once.
        assert!(
            ping_calls.load(Ordering::SeqCst) >= 1,
            "ping was never called"
        );

        // On the timeout path, now() and set_clock must NOT be called — there
        // is no point capturing a timestamp or setting the clock if the agent
        // never came up.
        assert!(
            !now_called.load(Ordering::SeqCst),
            "now() must not be called on the timeout path"
        );
        assert!(
            !set_clock_called.load(Ordering::SeqCst),
            "set_clock must not be called on the timeout path"
        );

        // The elapsed time should be at least the timeout (minus a small
        // tolerance for scheduling), confirming the loop actually waited.
        assert!(
            elapsed >= timeout.saturating_sub(Duration::from_millis(50)),
            "elapsed {:?} is less than the timeout {:?} — the loop did not \
             wait for the deadline",
            elapsed,
            timeout
        );
    }

    /// Unit test: `sync_one_child_clock` propagates a `set_clock` failure.
    /// Covers the set_clock-failure path flagged by the
    /// testing/reliability/adversarial reviewers. The probe succeeds
    /// immediately, `now()` is captured, but `set_clock` returns an error —
    /// that error must propagate verbatim and `now()` must have been called
    /// (the timestamp was captured before the failed set).
    #[test]
    fn sync_one_child_clock_propagates_set_clock_failure() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let now_called = Arc::new(AtomicBool::new(false));

        // Probe succeeds immediately.
        let ping = || Ok(());

        let now_called_cl = now_called.clone();
        let now = || {
            now_called_cl.store(true, Ordering::SeqCst);
            1_700_000_000 // deterministic fixed timestamp
        };

        // Clock-setter always fails.
        let set_clock = |_ts: u64| bail!("date -s exited with 1: operation not permitted");

        let result = sync_one_child_clock(ping, now, set_clock, CLOCK_SYNC_AGENT_TIMEOUT);

        let err = result.expect_err("expected set_clock failure to propagate");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("date -s exited with 1"),
            "set_clock error should propagate verbatim, got: {msg}"
        );

        // now() must have been called (timestamp captured before set_clock).
        assert!(
            now_called.load(Ordering::SeqCst),
            "now() must be called before set_clock, even when set_clock fails"
        );
    }

    // --- #301: per-VM partial-failure reporting -----------------------------
    //
    // The RestoreError/RestoreFailure types are pure data — they can be
    // constructed and formatted without a firecracker process. These tests
    // verify the Display contract and the phase enum so the structured
    // error carries enough context for callers to report which child failed.

    #[test]
    fn restore_error_display_lists_all_failures() {
        let err = RestoreError {
            failures: vec![
                RestoreFailure {
                    child_index: 2,
                    phase: RestorePhase::SocketWait,
                    pid: Some(4455),
                    error: "socket never appeared within 10s".to_string(),
                },
                RestoreFailure {
                    child_index: 4,
                    phase: RestorePhase::Restore,
                    pid: Some(4457),
                    error: "api_call: PUT /snapshot/load returned 400".to_string(),
                },
            ],
        };
        let s = format!("{err}");
        assert!(
            s.contains("2 child(ren) failed"),
            "display should report count: {s}"
        );
        assert!(s.contains("child 2"), "display should name child 2: {s}");
        assert!(
            s.contains("SocketWait"),
            "display should name SocketWait phase: {s}"
        );
        assert!(s.contains("pid 4455"), "display should include pid: {s}");
        assert!(s.contains("child 4"), "display should name child 4: {s}");
        assert!(
            s.contains("Restore"),
            "display should name Restore phase: {s}"
        );
        assert!(s.contains("400"), "display should include error: {s}");
    }

    #[test]
    fn restore_error_single_failure_display() {
        let err = RestoreError {
            failures: vec![RestoreFailure {
                child_index: 1,
                phase: RestorePhase::Spawn,
                pid: None,
                error: "failed to spawn firecracker".to_string(),
            }],
        };
        let s = format!("{err}");
        assert!(s.contains("1 child(ren) failed"), "{s}");
        assert!(s.contains("child 1"), "{s}");
        assert!(s.contains("Spawn"), "{s}");
        // Spawn-phase failures have no pid.
        assert!(!s.contains("pid"), "Spawn failure should not show pid: {s}");
    }

    #[test]
    fn restore_phase_eq_distinguishes_phases() {
        assert_ne!(RestorePhase::Spawn, RestorePhase::SocketWait);
        assert_ne!(RestorePhase::SocketWait, RestorePhase::Restore);
        assert_ne!(RestorePhase::Spawn, RestorePhase::Restore);
        assert_eq!(RestorePhase::Spawn, RestorePhase::Spawn);
    }

    // --- #301: pre-restore orphan detection ---------------------------------
    //
    // `scan_firecracker_orphans` is a read-only `/proc` scan on Linux and
    // a no-op stub on non-Linux. The non-Linux stub contract is the only
    // part testable without `/proc`: it must return an empty vec for any
    // input (including a known-pid set that would otherwise exclude
    // everything) so dev-box callers (macOS) don't panic.
    //
    // Non-Linux-gated: on Linux the real /proc scan runs and its result
    // depends on which firecracker processes happen to be alive on the
    // host, which is non-deterministic in CI and on dev boxes.
    #[test]
    #[cfg(not(target_os = "linux"))]
    fn scan_firecracker_orphans_non_linux_returns_empty() {
        let known: std::collections::HashSet<u32> = [1u32, 2, 3].into_iter().collect();
        let orphans = scan_firecracker_orphans(&known);
        assert!(
            orphans.is_empty(),
            "non-Linux stub should return no orphans, got {orphans:?}"
        );
    }

    #[test]
    #[cfg(not(target_os = "linux"))]
    fn scan_firecracker_orphans_empty_known_set_non_linux() {
        let known: std::collections::HashSet<u32> = std::collections::HashSet::new();
        let orphans = scan_firecracker_orphans(&known);
        assert!(orphans.is_empty());
    }

    // OrphanProcess and OrphanProcess fields are pure data — verify the
    // struct can be constructed with expected fields so downstream logging
    // and serialization (if added later) have a stable shape.
    #[test]
    fn orphan_process_struct_shape() {
        let o = OrphanProcess {
            pid: 12345,
            elapsed_secs: 3600,
            cmdline: "firecracker --api-sock /tmp/x.sock".to_string(),
        };
        assert_eq!(o.pid, 12345);
        assert_eq!(o.elapsed_secs, 3600);
        assert!(o.cmdline.contains("api-sock"));
    }

    /// Integration test: restore_many_with preserves successful siblings
    /// when one child fails to spawn. Boots a real VM, snapshots it, then
    /// restores two children with per-child netns. Child 1's netns is
    /// pre-provisioned so it spawns and restores normally; child 2's netns
    /// is deliberately left un-provisioned so `ip netns exec` fails at
    /// spawn time. Child 2 is dropped (Spawn-phase failure) while child 1
    /// survives.
    ///
    /// This exercises the partial-success contract: the result carries
    /// one live child (with its child_index preserved) and one Spawn
    /// failure, not an all-or-nothing error. Uses per_child_netns=true
    /// (valid multi-child topology) per review feedback on #301.
    #[test]
    #[ignore = "requires Linux + KVM + firecracker + rootfs image"]
    #[cfg(target_os = "linux")]
    fn restore_many_partial_spawn_failure_keeps_siblings() {
        let kernel = std::env::var("FORKD_TEST_KERNEL")
            .unwrap_or_else(|_| "/var/lib/forkd/kernels/vmlinux".to_string());
        let rootfs = std::env::var("FORKD_TEST_ROOTFS")
            .expect("FORKD_TEST_ROOTFS must point to an ext4 rootfs image");

        assert!(
            std::path::Path::new(&kernel).exists(),
            "FORKD_TEST_KERNEL not found at {kernel}"
        );
        assert!(
            std::path::Path::new(&rootfs).exists(),
            "FORKD_TEST_ROOTFS not found at {rootfs}"
        );

        let work_dir =
            std::env::temp_dir().join(format!("forkd-restore-partial-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&work_dir);
        let addr = "10.42.0.2:8888".to_string();

        // Pre-flight: no stale VM should already occupy the agent address.
        assert!(
            ping_at(&addr).is_err(),
            "{addr} is already reachable — a stale VM may occupy the address"
        );

        // Boot a parent VM and snapshot it.
        let cfg = BootConfig::ext4_rw(kernel.into(), rootfs.into(), work_dir.clone())
            .with_network(NetworkConfig::default_tap("forkd-tap0"));
        let vm = Vm::boot(&cfg).expect("boot parent VM");
        let mut connected = false;
        for _ in 0..60 {
            std::thread::sleep(Duration::from_millis(500));
            if ping_at(&addr).is_ok() {
                connected = true;
                break;
            }
        }
        assert!(connected, "parent VM agent did not respond within 30s");

        let snap_dir = work_dir.join("snap");
        std::fs::create_dir_all(&snap_dir).expect("create snap dir");
        vm.pause().expect("pause parent");
        let snapshot = vm
            .snapshot_to(
                snap_dir.join("vmstate"),
                snap_dir.join("memory.bin"),
                Vec::new(),
            )
            .expect("snapshot parent");
        drop(vm); // kills parent, frees the shared guest agent address

        // Restore two children with per-child netns (valid multi-child
        // topology). Child 1's netns is pre-provisioned; child 2's is
        // deliberately left un-provisioned so `ip netns exec` fails and the
        // spawn is recorded as a Spawn-phase failure.
        let netns_offset = std::process::id() as usize * 10;
        let child1_netns = format!("forkd-child-{}", netns_offset + 1);
        // Child 2 netns (forkd-child-{netns_offset+2}) is deliberately NOT
        // provisioned — that is the failure injection (spawn fails).

        // Pre-provision child 1's netns only.
        let netns_created = std::process::Command::new("ip")
            .args(["netns", "add", &child1_netns])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !netns_created {
            // Cannot create netns (not root or ip not available) — skip.
            // Clean up the parent snapshot files we created.
            let _ = std::fs::remove_dir_all(&work_dir);
            eprintln!("skipped: could not create netns {child1_netns} (need root)");
            return;
        }

        let restore_dir = work_dir.join("restore");
        std::fs::create_dir_all(&restore_dir).expect("create restore dir");

        let result = snapshot
            .restore_many_with(
                ForkOpts {
                    n: 2,
                    per_child_netns: true,
                    netns_offset,
                    socket_wait_timeout_secs: 10,
                    ..ForkOpts::default()
                },
                &restore_dir,
            )
            .expect("restore_many_with returned a fatal error");

        // Partial success: child 1 alive (with child_index=1), child 2
        // failed at Spawn (netns does not exist).
        assert_eq!(
            result.children.len(),
            1,
            "expected 1 surviving child, got {}",
            result.children.len()
        );
        assert_eq!(
            result.children[0].child_index, 1,
            "surviving child should retain child_index=1"
        );
        assert_eq!(
            result.failures.len(),
            1,
            "expected 1 failure, got {:?}",
            result.failures
        );
        let fail = &result.failures[0];
        assert_eq!(
            fail.child_index, 2,
            "expected child 2 to fail, got {:?}",
            fail
        );
        assert!(
            matches!(fail.phase, RestorePhase::Spawn),
            "expected Spawn-phase failure (netns not provisioned), got {:?}",
            fail.phase
        );

        // The surviving child is actually a live firecracker process.
        let child = &result.children[0].vm;
        assert!(
            child.is_alive(),
            "surviving child (pid {}) should be alive",
            child.pid()
        );

        drop(result);
        let _ = std::fs::remove_dir_all(&work_dir);
        // Clean up the pre-provisioned netns.
        let _ = std::process::Command::new("ip")
            .args(["netns", "del", &child1_netns])
            .status();
    }
}
