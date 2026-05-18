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
pub mod paths;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
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
    pub fn with_volume(mut self, volume: VolumeSpec) -> Self {
        // Volumes occupy /dev/vdb onwards (vda is rootfs); index i → vdN.
        let index = self.volumes.len();
        let dev = volume_device_name(index);
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
        self
    }
}

/// `0 → "vdb"`, `1 → "vdc"`, ... up to `23 → "vdy"`. After that, callers
/// hit virtio-blk's practical drive ceiling; we cap to keep the API simple.
pub fn volume_device_name(index: usize) -> String {
    let letter = (b'b' + index as u8) as char;
    format!("vd{letter}")
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

    /// Is the firecracker process still alive on the host?
    pub fn is_alive(&self) -> bool {
        Path::new(&format!("/proc/{}", self.pid)).exists()
    }
}

/// On-disk snapshot of a paused VM: a vmstate blob (vCPU + devices) plus a
/// memory image file. Children restore from these by mmap'ing memory with
/// `MAP_PRIVATE`, which the kernel implements as copy-on-write.
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
}

impl Default for ForkOpts {
    fn default() -> Self {
        Self {
            n: 1,
            per_child_netns: false,
            memory_limit_mib: None,
            netns_offset: 0,
            prewarm_scratch_dir: None,
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
            },
            work_dir,
        )
    }

    /// Same as `restore_many` but with explicit options.
    pub fn restore_many_with(&self, opts: ForkOpts, work_dir: &Path) -> Result<ForkResult> {
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

        // Phase 2: parallel restore via threads. Each thread issues one
        // /snapshot/load PUT to its child's API socket.
        let restore_start = Instant::now();
        let body = serde_json::json!({
            "snapshot_path": &self.vmstate,
            "mem_backend": {"backend_path": &self.memory, "backend_type": "File"},
            "enable_diff_snapshots": false,
            "resume_vm": true,
        })
        .to_string();

        let mut handles = Vec::with_capacity(n);
        for c in &children {
            let sock = c.sock.clone();
            let body = body.clone();
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
                    let resume_result =
                        api_call(&sock, "PATCH", "/vm", r#"{"state":"Resumed"}"#);
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
        assert_eq!(volume_device_name(0), "vdb");
        assert_eq!(volume_device_name(1), "vdc");
        assert_eq!(volume_device_name(22), "vdx");
    }

    #[test]
    fn boot_config_with_volume_appends_cmdline_hint() {
        let cfg = BootConfig::ext4_rw("/tmp/k".into(), "/tmp/r".into(), "/tmp/w".into())
            .with_volume(VolumeSpec {
                host_path: "/var/lib/forkd/vol/pyagent.img".into(),
                guest_path: "/opt/cache".into(),
                read_only: false,
            });
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
            .with_volume(VolumeSpec {
                host_path: "/b.img".into(),
                guest_path: "/opt/b".into(),
                read_only: true,
            });
        assert_eq!(cfg.volumes.len(), 2);
        assert!(
            cfg.boot_args.contains("forkd.mounts=vdb:/opt/a,vdc:/opt/b"),
            "boot_args was: {}",
            cfg.boot_args
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
    fn snapshot_serializes_round_trip() {
        let s = Snapshot {
            vmstate: "/tmp/v".into(),
            memory: "/tmp/m".into(),
            volumes: vec![VolumeSpec {
                host_path: "/var/lib/forkd/vol/pyagent.img".into(),
                guest_path: "/opt/cache".into(),
                read_only: false,
            }],
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: Snapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(s.vmstate, back.vmstate);
        assert_eq!(s.memory, back.memory);
        assert_eq!(s.volumes.len(), back.volumes.len());
        assert_eq!(s.volumes[0].guest_path, back.volumes[0].guest_path);
    }
}
