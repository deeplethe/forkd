//! Firecracker, the optional backend for hosts with `/dev/kvm`.
//!
//! NOT YET RUN ON KVM. None of the machines this is developed on has
//! `/dev/kvm`; everything below is compiled and unit-tested, not measured.
//! `docs/adr/0011-firecracker-backend.md` says what to verify first.
//!
//! Upstream Firecracker, no fork. What it keeps from v0.5:
//!
//!   - Forks share one page cache: every child of a tag maps the same
//!     memory file with the File backend, which is `MAP_PRIVATE`.
//!   - Diff snapshots: a sandbox restored from an image is snapshotted next
//!     time as only the pages it dirtied, chained onto that image.
//!   - A private disk per sandbox, reflinked, so no two VMs ever write one
//!     ext4 image.
//!
//! An image is a flat directory -- tags are transferred as flat files:
//!
//! ```text
//!   vmstate          devices and vCPUs
//!   mem.0            full memory
//!   mem.1 .. mem.k   diffs, sparse, each against everything before it
//!   rootfs.ext4      the disk as it was while the VM was paused
//!   .mem.assembled   mem.0 with the diffs applied; a local cache, never sent
//! ```
//!
//! Memory and disk travel together because the guest's page cache refers to
//! disk blocks: memory from one moment over the disk from another corrupts
//! the filesystem without any error at the time.
//!
//! The disk's path in the VM is the relative `rootfs.ext4`, and Firecracker
//! runs with the sandbox's own disk directory as its working directory. So
//! the path recorded in a vmstate resolves, in whichever sandbox loads it,
//! to that sandbox's own copy -- never the parent's, and with no need for
//! the parent's path to exist on this node.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command as Exec, Stdio};
use std::time::{Duration, Instant};

use super::backend::Backend;
use super::bundle::{self, BuildRequest, TemplateSpec};
use super::runtime::RuntimeConfig;

/// Every guest has the same address, behind a tap in its own namespace, so
/// the network a snapshot remembers is right in any namespace it is
/// restored into. The namespace's own address is DNATed to it.
const GUEST_IP: &str = "10.42.0.2";
const TAP_CIDR: &str = "10.42.0.1/30";
const TAP: &str = "tap0";
const GUEST_MAC: &str = "AA:FC:00:00:00:01";

/// The guest agent (`rootfs-init/sandstorm-agent.py`). A template cannot
/// use this port.
const AGENT_PORT: u16 = 8888;

/// Diffs on top of a full image before the next checkpoint starts again
/// from a full one. Each link costs a pass over its data at assembly.
const MAX_CHAIN: usize = 8;

/// Guest memory when the template does not say.
const DEFAULT_MEM_MIB: u64 = 512;
/// The cgroup's ceiling is the guest's memory plus this, for the VMM.
const VMM_OVERHEAD_MIB: u64 = 64;
/// x86's COMMAND_LINE_SIZE.
const CMDLINE_MAX: usize = 2048;

/// Calls that should answer at once.
const API_TIMEOUT: Duration = Duration::from_secs(10);
/// Snapshot create and load do work proportional to memory.
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(120);

pub struct Firecracker {
    pub bin: String,
    /// Uncompressed vmlinux.
    pub kernel: PathBuf,
    pub vcpus: u8,
    pub work_dir: PathBuf,
}

/// What `prepare` records in `bundles/<id>/config.json`, so every later
/// call can be made from the id alone after a restart.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct VmConfig {
    /// The network namespace's name, as `ip netns` knows it.
    netns: Option<String>,
    /// The namespace's address, which the guest is reached at.
    sandbox_ip: Option<Ipv4Addr>,
    mem_mib: u64,
    vcpus: u8,
    boot_args: String,
}

impl Firecracker {
    /// Refuse to run where this backend cannot work, rather than fail on
    /// every sandbox. ADR 11: no silent full copies.
    pub fn check_host(&self) -> Result<()> {
        if !Path::new("/dev/kvm").exists() {
            bail!("the firecracker backend needs /dev/kvm, and this host has none");
        }
        if !self.kernel.is_file() {
            bail!("no guest kernel at {}", self.kernel.display());
        }
        let probe = self.work_dir.join("tmp").join("reflink-probe");
        std::fs::create_dir_all(probe.parent().unwrap())?;
        std::fs::write(&probe, b"probe")?;
        let copy = probe.with_extension("copy");
        let r = reflink(&probe, &copy);
        std::fs::remove_file(&probe).ok();
        std::fs::remove_file(&copy).ok();
        r.with_context(|| {
            format!(
                "{} cannot reflink; the firecracker backend needs XFS (reflink=1) \
                 or btrfs, or every sandbox would copy its whole disk",
                self.work_dir.display()
            )
        })
    }

    fn bundle(&self, id: &str) -> PathBuf {
        self.work_dir.join("bundles").join(id)
    }

    fn disk_dir(&self, id: &str) -> PathBuf {
        self.work_dir.join("layers").join(id).join("disk")
    }

    /// The images this VM's memory came from, hard-linked: what the next
    /// checkpoint can be a diff against.
    fn base_dir(&self, id: &str) -> PathBuf {
        self.work_dir.join("layers").join(id).join("base")
    }

    fn sock(&self, id: &str) -> PathBuf {
        self.bundle(id).join("fc.sock")
    }

    fn config(&self, id: &str) -> Result<VmConfig> {
        let p = self.bundle(id).join("config.json");
        let raw = std::fs::read(&p).with_context(|| format!("reading {}", p.display()))?;
        serde_json::from_slice(&raw).with_context(|| format!("{} is not a VM config", p.display()))
    }

    /// Start Firecracker for this sandbox, detached from this process and
    /// inside the sandbox's cgroup from its first instruction.
    fn spawn(&self, id: &str, cfg: &VmConfig) -> Result<()> {
        let sock = self.sock(id);
        let _ = std::fs::remove_file(&sock);
        let disk = self.disk_dir(id);
        let logs = self.work_dir.join("logs");
        std::fs::create_dir_all(&logs)?;
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(logs.join(format!("{id}.log")))?;

        let cg = cgroup(id, cfg.mem_mib + VMM_OVERHEAD_MIB)?;
        let procs = std::fs::OpenOptions::new()
            .write(true)
            .open(cg.join("cgroup.procs"))
            .with_context(|| format!("opening {}/cgroup.procs", cg.display()))?;

        let mut cmd = match &cfg.netns {
            Some(ns) => {
                let mut c = Exec::new("ip");
                c.args(["netns", "exec", ns, &self.bin]);
                c
            }
            None => Exec::new(&self.bin),
        };
        cmd.arg("--api-sock")
            .arg(&sock)
            .current_dir(&disk)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        {
            use std::os::unix::io::AsRawFd;
            use std::os::unix::process::CommandExt;
            let fd = procs.as_raw_fd();
            // Its own session, so nothing aimed at this process's group
            // reaches it; and into the cgroup before exec, so there is no
            // moment when a freeze or a kill would miss it. `ip netns exec`
            // execs Firecracker in place, so the pid stays the same.
            unsafe {
                cmd.pre_exec(move || {
                    if libc::setsid() < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    if libc::write(fd, b"0".as_ptr().cast(), 1) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        let mut child = cmd
            .spawn()
            .with_context(|| format!("starting {} for {id}", self.bin))?;
        drop(procs);
        let pid = child.id();
        std::fs::write(self.bundle(id).join("fc.pid"), pid.to_string())?;
        // Reaped here while this process lives; after a restart it is
        // init's to reap. Never killed on drop: a node restart must leave
        // its sandboxes running.
        let _ = std::thread::Builder::new()
            .name("sandstorm-fc-reap".into())
            .spawn(move || {
                let _ = child.wait();
            });

        let deadline = Instant::now() + Duration::from_secs(5);
        while UnixStream::connect(&sock).is_err() {
            if Instant::now() > deadline || !alive(pid) {
                self.stop(id);
                bail!("firecracker for {id} did not open its API socket; see its log");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(())
    }

    /// Kill this sandbox's Firecracker, if it is still the process we
    /// started -- a pid on its own could have been reused.
    fn stop(&self, id: &str) {
        let pidf = self.bundle(id).join("fc.pid");
        let sock = self.sock(id);
        if let Some(pid) = std::fs::read_to_string(&pidf)
            .ok()
            .and_then(|s| s.trim().parse::<i32>().ok())
        {
            let ours = std::fs::read(format!("/proc/{pid}/cmdline"))
                .map(|c| String::from_utf8_lossy(&c).contains(sock.to_string_lossy().as_ref()))
                .unwrap_or(false);
            if ours {
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
                let deadline = Instant::now() + Duration::from_secs(2);
                while alive(pid as u32) && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
        std::fs::remove_file(&pidf).ok();
        std::fs::remove_file(&sock).ok();
    }

    fn ensure_net(&self, cfg: &VmConfig) -> Result<()> {
        match &cfg.netns {
            Some(ns) => ensure_tap(ns),
            None => Ok(()),
        }
    }
}

impl Backend for Firecracker {
    fn version(&self) -> String {
        Exec::new(&self.bin)
            .arg("--version")
            .output()
            .ok()
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .next()
                    .unwrap_or("")
                    .to_string()
            })
            .unwrap_or_default()
    }

    fn template_root(&self, dir: &Path) -> PathBuf {
        dir.join("rootfs.ext4")
    }

    fn prepare(&self, _: &RuntimeConfig, req: &BuildRequest<'_>) -> Result<PathBuf> {
        if req.volume.is_some() {
            bail!("volumes are not supported on the firecracker backend yet");
        }
        let disk = self.disk_dir(req.id);
        std::fs::create_dir_all(&disk)?;
        let rootfs = disk.join("rootfs.ext4");
        if !rootfs.exists() {
            match req.restore_from {
                Some(arc) => bundle::unpack_into(arc, &disk)
                    .with_context(|| format!("restoring {} from {}", req.id, arc.display()))?,
                None => reflink(req.template_rootfs, &rootfs).with_context(|| {
                    format!("giving {} its own copy of the template's disk", req.id)
                })?,
            }
        } else if req.restore_from.is_some() {
            bail!("{} already has a disk; it cannot be restored over", req.id);
        }

        let mem_mib = req.spec.memory_max_mib.unwrap_or(DEFAULT_MEM_MIB);
        let cfg = VmConfig {
            netns: req
                .netns
                .map(|p| p.rsplit('/').next().unwrap_or(p).to_string()),
            sandbox_ip: req.sandbox_ip,
            mem_mib,
            vcpus: self.vcpus,
            boot_args: boot_args(req.spec, req.port, req.netns.is_some())?,
        };
        let b = self.bundle(req.id);
        std::fs::create_dir_all(&b)?;
        std::fs::write(b.join("config.json"), serde_json::to_vec_pretty(&cfg)?)?;
        Ok(b)
    }

    fn layer(&self, _: &RuntimeConfig, id: &str) -> PathBuf {
        self.disk_dir(id)
    }

    fn unmount(&self, _: &RuntimeConfig, _: &str) -> Result<()> {
        Ok(())
    }

    fn teardown(&self, cfg: &RuntimeConfig, id: &str) -> Result<()> {
        self.stop(id);
        std::fs::remove_dir_all(cfg.work_dir.join("layers").join(id)).ok();
        std::fs::remove_dir_all(self.bundle(id)).ok();
        Ok(())
    }

    fn boot(&self, id: &str, _bundle: &Path) -> Result<()> {
        let cfg = self.config(id)?;
        self.ensure_net(&cfg)?;
        // A cold boot has no image under it, so its first checkpoint is full.
        std::fs::remove_dir_all(self.base_dir(id)).ok();
        self.spawn(id, &cfg)?;
        let sock = self.sock(id);
        let mut calls = vec![
            (
                "/boot-source",
                json!({ "kernel_image_path": self.kernel, "boot_args": cfg.boot_args }),
            ),
            (
                "/drives/rootfs",
                json!({
                    "drive_id": "rootfs",
                    "path_on_host": "rootfs.ext4",
                    "is_root_device": true,
                    "is_read_only": false,
                }),
            ),
            (
                "/machine-config",
                json!({
                    "vcpu_count": cfg.vcpus,
                    "mem_size_mib": cfg.mem_mib,
                    // Without it there are no diff snapshots of this VM.
                    "track_dirty_pages": true,
                }),
            ),
        ];
        if cfg.netns.is_some() {
            calls.push((
                "/network-interfaces/eth0",
                json!({ "iface_id": "eth0", "host_dev_name": TAP, "guest_mac": GUEST_MAC }),
            ));
        }
        calls.push(("/entropy", json!({})));
        calls.push(("/actions", json!({ "action_type": "InstanceStart" })));
        let r = calls
            .iter()
            .try_for_each(|(path, body)| api(&sock, "PUT", path, body, API_TIMEOUT).map(drop));
        if let Err(e) = r {
            self.stop(id);
            return Err(e).with_context(|| format!("booting {id}"));
        }
        Ok(())
    }

    fn restore(&self, id: &str, _bundle: &Path, image: &Path) -> Result<()> {
        let cfg = self.config(id)?;
        self.ensure_net(&cfg)?;

        // This VM's disk becomes the one taken with the memory it is about
        // to load. A reflink, so free; tmp and rename, so a failure leaves
        // the old disk rather than half of one.
        let disk = self.disk_dir(id);
        std::fs::create_dir_all(&disk)?;
        let tmp = disk.join("rootfs.ext4.tmp");
        let _ = std::fs::remove_file(&tmp);
        reflink(&image.join("rootfs.ext4"), &tmp)
            .with_context(|| format!("taking {id}'s disk from {}", image.display()))?;
        std::fs::rename(&tmp, disk.join("rootfs.ext4"))?;

        let chain = chain_of(image)?;
        let mem = assemble(image, &chain)?;
        link_chain(image, &chain, &self.base_dir(id))?;

        self.spawn(id, &cfg)?;
        let r = load(
            &self.sock(id),
            &image.join("vmstate"),
            &mem,
            &disk.join("rootfs.ext4"),
        );
        if let Err(e) = r {
            // Never left half-loaded, and never resumed on a disk that was
            // not re-pointed.
            self.stop(id);
            return Err(e).with_context(|| format!("restoring {id} from {}", image.display()));
        }
        if let Some(ip) = cfg.sandbox_ip {
            sync_clock(ip);
        }
        Ok(())
    }

    fn validate(&self, image: &Path) -> Result<()> {
        for name in ["vmstate", "rootfs.ext4"] {
            let len = std::fs::metadata(image.join(name))
                .with_context(|| format!("no {name} in {}", image.display()))?
                .len();
            if len == 0 {
                bail!("{name} in {} is empty", image.display());
            }
        }
        chain_of(image).map(drop)
    }

    fn checkpoint(&self, id: &str, image: &Path, _compress: bool) -> Result<()> {
        let sock = self.sock(id);
        api(
            &sock,
            "PATCH",
            "/vm",
            &json!({ "state": "Paused" }),
            API_TIMEOUT,
        )
        .with_context(|| format!("pausing {id}"))?;

        let r = (|| -> Result<()> {
            let base = self.base_dir(id);
            let have = chain_of(&base).unwrap_or_default();
            let (kind, next) = if !have.is_empty() && have.len() < MAX_CHAIN {
                link_chain(&base, &have, image)?;
                ("Diff", have.len())
            } else {
                ("Full", 0)
            };
            api(
                &sock,
                "PUT",
                "/snapshot/create",
                &json!({
                    "snapshot_type": kind,
                    "snapshot_path": image.join("vmstate"),
                    "mem_file_path": image.join(format!("mem.{next}")),
                }),
                SNAPSHOT_TIMEOUT,
            )?;
            // Paused, so the disk is exactly what the memory expects.
            reflink(
                &self.disk_dir(id).join("rootfs.ext4"),
                &image.join("rootfs.ext4"),
            )?;
            tracing::info!(%id, kind, links = next + 1, "firecracker snapshot");
            Ok(())
        })();
        match r {
            Ok(()) => {
                self.stop(id);
                Ok(())
            }
            Err(e) => {
                // Leave it running rather than paused for ever.
                let _ = api(
                    &sock,
                    "PATCH",
                    "/vm",
                    &json!({ "state": "Resumed" }),
                    API_TIMEOUT,
                );
                Err(e).with_context(|| format!("snapshotting {id}"))
            }
        }
    }

    fn remove(&self, id: &str) {
        self.stop(id);
    }

    fn kill(&self, id: &str) {
        self.stop(id);
    }

    fn exec(&self, id: &str, args: &[String], timeout: Duration) -> Result<(String, String, i64)> {
        if args.is_empty() {
            bail!("exec needs a command to run");
        }
        let ip = self
            .config(id)?
            .sandbox_ip
            .with_context(|| format!("{id} has no network, so its agent cannot be reached"))?;
        let resp = agent(
            ip,
            &json!({ "action": "exec", "args": args, "timeout": timeout.as_secs().max(1) }),
            timeout + Duration::from_secs(5),
        );
        let resp = match resp {
            Ok(r) => r,
            Err(e) if is_timeout(&e) => {
                return Ok((
                    String::new(),
                    format!("\nsandstorm: killed after {}s\n", timeout.as_secs()),
                    124,
                ))
            }
            Err(e) => return Err(e).with_context(|| format!("exec in {id}")),
        };
        if let Some(err) = resp.get("error").and_then(Value::as_str) {
            bail!("the agent in {id} refused the exec: {err}");
        }
        let s = |k: &str| {
            resp.get(k)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        let code = resp.get("exit_code").and_then(Value::as_i64).unwrap_or(-1);
        Ok((s("stdout"), s("stderr"), code))
    }
}

/// The kernel command line. The template's program goes in as hex JSON,
/// which `sandstorm-agent.py` starts beside itself.
fn boot_args(spec: &TemplateSpec, port: u16, net: bool) -> Result<String> {
    let args: Vec<String> = spec
        .args
        .iter()
        .map(|a| a.replace("{{PORT}}", &port.to_string()))
        .collect();
    let app = json!({ "args": args, "env": spec.env, "cwd": spec.cwd });
    let mut s = String::from(
        "console=ttyS0 reboot=k panic=1 pci=off clocksource=kvm-clock init=/sandstorm-init.sh",
    );
    if net {
        s.push_str(&format!(
            " ip={GUEST_IP}::10.42.0.1:255.255.255.252::eth0:off"
        ));
    }
    s.push_str(" sandstorm.app=");
    s.push_str(&hex::encode(serde_json::to_vec(&app)?));
    if s.len() >= CMDLINE_MAX {
        bail!(
            "the template's args and env make a {}-byte kernel command line; \
             the limit is {CMDLINE_MAX}",
            s.len()
        );
    }
    Ok(s)
}

/// Load a snapshot without running it, point the disk at this sandbox's
/// own, then run it -- the order PR #321 established. The relative disk path
/// already resolves to this sandbox's copy; the re-point is the second
/// guarantee, and the guest executes nothing until both have happened.
fn load(sock: &Path, vmstate: &Path, mem: &Path, disk: &Path) -> Result<()> {
    api(
        sock,
        "PUT",
        "/snapshot/load",
        &json!({
            "snapshot_path": vmstate,
            "mem_backend": { "backend_type": "File", "backend_path": mem },
            "enable_diff_snapshots": true,
            "resume_vm": false,
        }),
        SNAPSHOT_TIMEOUT,
    )
    .context("loading the snapshot")?;
    api(
        sock,
        "PATCH",
        "/drives/rootfs",
        &json!({ "drive_id": "rootfs", "path_on_host": disk }),
        API_TIMEOUT,
    )
    .context("pointing the disk at this sandbox's own")?;
    api(
        sock,
        "PATCH",
        "/vm",
        &json!({ "state": "Resumed" }),
        API_TIMEOUT,
    )
    .context("resuming")?;
    Ok(())
}

/// The memory files of an image, in chain order: `mem.0`, `mem.1`, ...
/// A gap means a broken image, which is refused rather than loaded short.
fn chain_of(dir: &Path) -> Result<Vec<String>> {
    let mut idx: Vec<usize> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .flatten()
        .filter_map(|e| {
            e.file_name()
                .to_str()?
                .strip_prefix("mem.")?
                .parse::<usize>()
                .ok()
        })
        .collect();
    idx.sort_unstable();
    if idx.is_empty() {
        bail!("{} has no memory image", dir.display());
    }
    if idx.iter().enumerate().any(|(i, &n)| i != n) {
        bail!("{} has a gap in its memory chain: {idx:?}", dir.display());
    }
    Ok(idx.into_iter().map(|i| format!("mem.{i}")).collect())
}

/// Hard-link a chain from one directory into another, replacing whatever
/// chain was there. Links, not copies or pointers: each image stays whole
/// on its own, so dropping a tag cannot break one derived from it.
fn link_chain(from: &Path, chain: &[String], to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(to)?.flatten() {
        if e.file_name().to_string_lossy().starts_with("mem.") {
            std::fs::remove_file(e.path())?;
        }
    }
    for f in chain {
        std::fs::hard_link(from.join(f), to.join(f))
            .with_context(|| format!("linking {f} into {}", to.display()))?;
    }
    Ok(())
}

/// The one memory file to load: `mem.0` itself, or the chain applied to a
/// reflink of it, cached in the image so every later fork maps the same
/// inode. The cache is a dot file, which tag transfer leaves out.
fn assemble(image: &Path, chain: &[String]) -> Result<PathBuf> {
    if chain.len() == 1 {
        return Ok(image.join(&chain[0]));
    }
    let out = image.join(".mem.assembled");
    if out.exists() {
        return Ok(out);
    }
    let tmp = image.join(format!(".mem.assembled.{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    reflink(&image.join(&chain[0]), &tmp)?;
    for f in &chain[1..] {
        apply_diff(&image.join(f), &tmp).with_context(|| format!("applying {f}"))?;
    }
    std::fs::rename(&tmp, &out)?;
    Ok(out)
}

/// Write each data range of a sparse diff over the same range of `base`.
/// A Firecracker Diff snapshot is the guest's whole address space with only
/// the dirtied pages present, so its holes are exactly what did not change.
#[cfg(target_os = "linux")]
fn apply_diff(diff: &Path, base: &Path) -> Result<()> {
    use std::io::{Seek, SeekFrom};
    use std::os::unix::io::AsRawFd;
    let mut d = std::fs::File::open(diff)?;
    let mut b = std::fs::OpenOptions::new().write(true).open(base)?;
    let len = d.metadata()?.len();
    if b.metadata()?.len() != len {
        bail!("{} and {} differ in size", diff.display(), base.display());
    }
    let mut buf = vec![0u8; 1 << 20];
    let mut at: i64 = 0;
    while (at as u64) < len {
        let start = unsafe { libc::lseek(d.as_raw_fd(), at, libc::SEEK_DATA) };
        if start < 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::ENXIO) {
                break; // no data past `at`
            }
            return Err(e.into());
        }
        let end = unsafe { libc::lseek(d.as_raw_fd(), start, libc::SEEK_HOLE) };
        if end < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        d.seek(SeekFrom::Start(start as u64))?;
        b.seek(SeekFrom::Start(start as u64))?;
        let mut left = (end - start) as usize;
        while left > 0 {
            let take = left.min(buf.len());
            let n = d.read(&mut buf[..take])?;
            if n == 0 {
                break;
            }
            b.write_all(&buf[..n])?;
            left -= n;
        }
        at = end;
    }
    b.sync_data()?;
    Ok(())
}

/// Elsewhere there is no SEEK_DATA to trust; fall back to writing every
/// non-zero page, which is what a hole reads as. Only for tests: the
/// backend itself runs on Linux.
#[cfg(not(target_os = "linux"))]
fn apply_diff(diff: &Path, base: &Path) -> Result<()> {
    use std::io::{Seek, SeekFrom};
    let mut d = std::fs::File::open(diff)?;
    let mut b = std::fs::OpenOptions::new().write(true).open(base)?;
    if b.metadata()?.len() != d.metadata()?.len() {
        bail!("{} and {} differ in size", diff.display(), base.display());
    }
    let mut page = vec![0u8; 4096];
    let mut at = 0u64;
    loop {
        let n = d.read(&mut page)?;
        if n == 0 {
            break;
        }
        if page[..n].iter().any(|&x| x != 0) {
            b.seek(SeekFrom::Start(at))?;
            b.write_all(&page[..n])?;
        }
        at += n as u64;
    }
    Ok(())
}

/// A copy-on-write clone, or an error. Never a silent full copy: that is
/// how v0.5 lost its density without anyone noticing (#322).
fn reflink(src: &Path, dst: &Path) -> Result<()> {
    let s = std::fs::File::open(src).with_context(|| format!("opening {}", src.display()))?;
    let d = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dst)
        .with_context(|| format!("creating {}", dst.display()))?;
    let r = clone_file(&s, &d);
    if r.is_err() {
        drop(d);
        std::fs::remove_file(dst).ok();
    }
    r.with_context(|| format!("reflinking {} to {}", src.display(), dst.display()))
}

/// _IOW(0x94, 9, int). v0.5 had this number wrong (#322) and fell back to
/// copying on every filesystem; a test pins it.
#[cfg(target_os = "linux")]
const FICLONE: libc::c_ulong = 0x4004_9409;

#[cfg(target_os = "linux")]
fn clone_file(src: &std::fs::File, dst: &std::fs::File) -> Result<()> {
    use std::os::unix::io::AsRawFd;
    if unsafe { libc::ioctl(dst.as_raw_fd(), FICLONE as _, src.as_raw_fd()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

/// macOS has clonefile(2) on APFS, but not on an open descriptor; tests
/// get a plain copy so the chain logic can be exercised here.
#[cfg(not(target_os = "linux"))]
fn clone_file(src: &std::fs::File, dst: &std::fs::File) -> Result<()> {
    let (mut s, mut d) = (src, dst);
    std::io::copy(&mut s, &mut d)?;
    Ok(())
}

/// Put the tap and the two NAT rules into the sandbox's namespace. Every
/// step checks first, so this is safe to run again before every boot and
/// restore -- which it must, since Snapshot releases the network.
fn ensure_tap(ns: &str) -> Result<()> {
    let ok = |args: &[&str]| {
        Exec::new(args[0])
            .args(&args[1..])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };
    let run = |what: &str, args: &[&str]| -> Result<()> {
        let out = Exec::new(args[0])
            .args(&args[1..])
            .output()
            .with_context(|| format!("running {args:?}"))?;
        if !out.status.success() {
            bail!("{what}: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(())
    };
    if !ok(&["ip", "-n", ns, "link", "show", TAP]) {
        run(
            "creating the tap",
            &["ip", "-n", ns, "tuntap", "add", "dev", TAP, "mode", "tap"],
        )?;
    }
    run(
        "addressing the tap",
        &["ip", "-n", ns, "addr", "replace", TAP_CIDR, "dev", TAP],
    )?;
    run("tap up", &["ip", "-n", ns, "link", "set", TAP, "up"])?;
    run(
        "forwarding in the namespace",
        &[
            "ip",
            "netns",
            "exec",
            ns,
            "sysctl",
            "-qw",
            "net.ipv4.ip_forward=1",
        ],
    )?;
    // Everything that arrives from outside goes to the guest; what the
    // guest sends leaves as the namespace's own address, so the node's
    // rules for 10.86/16 apply to it unchanged.
    let rules: [&[&str]; 2] = [
        &[
            "PREROUTING",
            "!",
            "-i",
            TAP,
            "-j",
            "DNAT",
            "--to-destination",
            GUEST_IP,
        ],
        &[
            "POSTROUTING",
            "-s",
            GUEST_IP,
            "!",
            "-o",
            TAP,
            "-j",
            "MASQUERADE",
        ],
    ];
    for rule in rules {
        let mut check = vec!["ip", "netns", "exec", ns, "iptables", "-t", "nat", "-C"];
        check.extend_from_slice(rule);
        if !ok(&check) {
            let mut add = check.clone();
            add[7] = "-A";
            run("adding a NAT rule", &add)?;
        }
    }
    Ok(())
}

/// The sandbox's cgroup, created with its memory ceiling. The same path
/// runsc's bundle names, so the node's freeze, accounting and kill work on
/// a VM unchanged.
fn cgroup(id: &str, max_mib: u64) -> Result<PathBuf> {
    let parent = Path::new("/sys/fs/cgroup/sandstorm");
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    // Controllers runsc would otherwise have enabled. Already-enabled ones
    // are no error; a missing one shows up as the memory.max write failing.
    let _ = std::fs::write(parent.join("cgroup.subtree_control"), "+memory +cpu +pids");
    let cg = parent.join(id);
    std::fs::create_dir_all(&cg)?;
    std::fs::write(cg.join("memory.max"), (max_mib << 20).to_string())
        .with_context(|| format!("setting {}/memory.max", cg.display()))?;
    Ok(cg)
}

fn alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// One request to the guest agent: a JSON line out, a JSON line back.
fn agent(ip: Ipv4Addr, req: &Value, timeout: Duration) -> Result<Value> {
    let addr = SocketAddr::from((ip, AGENT_PORT));
    // Not an io::Error once it leaves here: a connect that times out is an
    // agent that is not there, not a command that ran too long.
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(2))
        .map_err(|e| anyhow::anyhow!("cannot reach the agent at {addr}: {e}"))?;
    s.set_read_timeout(Some(timeout))?;
    s.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut line = serde_json::to_vec(req)?;
    line.push(b'\n');
    s.write_all(&line)?;
    s.shutdown(std::net::Shutdown::Write)?;
    let mut out = Vec::new();
    s.read_to_end(&mut out)?;
    let first = out.split(|&b| b == b'\n').next().unwrap_or(&[]);
    serde_json::from_slice(first).context("the agent's answer is not JSON")
}

fn is_timeout(e: &anyhow::Error) -> bool {
    e.downcast_ref::<std::io::Error>().is_some_and(|io| {
        matches!(
            io.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        )
    })
}

/// A restored guest's clock is wherever it was when the image was taken.
/// Best effort and bounded: a sandbox whose clock could not be set is still
/// better served than one that was not restored.
fn sync_clock(ip: Ipv4Addr) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if agent(ip, &json!({ "action": "ping" }), Duration::from_millis(500)).is_ok() {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            let req = json!({
                "action": "exec",
                "args": ["date", "-s", format!("@{now}")],
                "timeout": 2,
            });
            if let Err(e) = agent(ip, &req, Duration::from_secs(3)) {
                tracing::warn!(%ip, error = %e, "could not set a restored guest's clock");
            }
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    tracing::warn!(%ip, "a restored guest's agent did not answer; its clock is stale");
}

/// One call to Firecracker's API over its Unix socket. Returns the body;
/// any status outside 2xx is an error carrying Firecracker's own message.
fn api(sock: &Path, method: &str, path: &str, body: &Value, timeout: Duration) -> Result<String> {
    let mut s =
        UnixStream::connect(sock).with_context(|| format!("connecting to {}", sock.display()))?;
    s.set_read_timeout(Some(timeout))?;
    s.set_write_timeout(Some(timeout))?;
    let body = serde_json::to_vec(body)?;
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nAccept: application/json\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes())?;
    s.write_all(&body)?;

    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let (head_end, want) = loop {
        let n = s
            .read(&mut chunk)
            .with_context(|| format!("{method} {path}: no answer"))?;
        if n == 0 {
            bail!("{method} {path}: the connection closed before an answer");
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..i]).to_string();
            let len = head
                .lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    k.eq_ignore_ascii_case("content-length")
                        .then(|| v.trim().parse::<usize>().ok())?
                })
                .unwrap_or(0);
            break (i + 4, len);
        }
    };
    while buf.len() < head_end + want {
        let n = s.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let status_line =
        String::from_utf8_lossy(&buf[..buf.iter().position(|&b| b == b'\r').unwrap_or(0)])
            .to_string();
    let code: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .with_context(|| format!("{method} {path}: unreadable status {status_line:?}"))?;
    let resp = String::from_utf8_lossy(&buf[head_end..]).to_string();
    if !(200..300).contains(&code) {
        bail!("{method} {path} -> {code}: {}", resp.trim());
    }
    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;

    /// A stand-in for Firecracker's API: records each request line and
    /// answers with the next status, or 204.
    fn fake_api(dir: &Path, statuses: Vec<u16>) -> (PathBuf, Arc<Mutex<Vec<String>>>) {
        let sock = dir.join("fc.sock");
        let l = UnixListener::bind(&sock).unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let rec = seen.clone();
        std::thread::spawn(move || {
            let mut statuses = statuses.into_iter();
            for conn in l.incoming() {
                let mut c = conn.unwrap();
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                // Read the head, then the body by its length.
                let (end, len) = loop {
                    let n = c.read(&mut chunk).unwrap();
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let h = String::from_utf8_lossy(&buf[..i]).to_string();
                        let len = h
                            .lines()
                            .find_map(|l| l.strip_prefix("Content-Length: "))
                            .and_then(|v| v.parse::<usize>().ok())
                            .unwrap_or(0);
                        break (i + 4, len);
                    }
                };
                while buf.len() < end + len {
                    let n = c.read(&mut chunk).unwrap();
                    buf.extend_from_slice(&chunk[..n]);
                }
                let line =
                    String::from_utf8_lossy(&buf[..buf.iter().position(|&b| b == b'\r').unwrap()])
                        .to_string();
                let body = String::from_utf8_lossy(&buf[end..end + len]).to_string();
                rec.lock().unwrap().push(format!("{line} {body}"));
                let code = statuses.next().unwrap_or(204);
                let msg = if code >= 300 {
                    r#"{"fault_message":"no"}"#
                } else {
                    ""
                };
                write!(
                    c,
                    "HTTP/1.1 {code} X\r\nContent-Length: {}\r\n\r\n{msg}",
                    msg.len()
                )
                .unwrap();
            }
        });
        (sock, seen)
    }

    #[test]
    fn the_api_client_reads_status_and_body() {
        let dir = TempDir::new().unwrap();
        let (sock, seen) = fake_api(dir.path(), vec![204, 400]);
        api(&sock, "PUT", "/actions", &json!({"a": 1}), API_TIMEOUT).unwrap();
        let e = api(&sock, "PATCH", "/vm", &json!({}), API_TIMEOUT).unwrap_err();
        assert!(
            e.to_string().contains("400") && e.to_string().contains("no"),
            "{e}"
        );
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0], r#"PUT /actions HTTP/1.1 {"a":1}"#);
    }

    /// The order that keeps a fork off its parent's disk: nothing runs
    /// until the drive has been re-pointed, and a failed re-point never
    /// resumes.
    #[test]
    fn a_load_repoints_the_disk_before_it_resumes() {
        let dir = TempDir::new().unwrap();
        let (sock, seen) = fake_api(dir.path(), vec![]);
        load(
            &sock,
            Path::new("/i/vmstate"),
            Path::new("/i/mem.0"),
            Path::new("/d/rootfs.ext4"),
        )
        .unwrap();
        let seen = seen.lock().unwrap();
        assert!(seen[0].starts_with("PUT /snapshot/load"));
        assert!(seen[0].contains(r#""resume_vm":false"#));
        assert!(seen[0].contains(r#""backend_type":"File""#));
        assert!(seen[1].starts_with("PATCH /drives/rootfs"));
        assert!(seen[1].contains("/d/rootfs.ext4"));
        assert_eq!(seen[2], r#"PATCH /vm HTTP/1.1 {"state":"Resumed"}"#);
        assert_eq!(seen.len(), 3);
    }

    #[test]
    fn a_failed_repoint_never_resumes() {
        let dir = TempDir::new().unwrap();
        let (sock, seen) = fake_api(dir.path(), vec![204, 400]);
        assert!(load(&sock, Path::new("/v"), Path::new("/m"), Path::new("/d")).is_err());
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert!(!seen.iter().any(|l| l.contains("Resumed")));
    }

    /// Whether this filesystem can reflink. CI's ext4 cannot, and the
    /// backend refuses to copy instead, so tests that need it say so and
    /// stop rather than fail.
    fn reflinks(dir: &Path) -> bool {
        let (a, b) = (dir.join(".probe-a"), dir.join(".probe-b"));
        std::fs::write(&a, "x").unwrap();
        let ok = reflink(&a, &b).is_ok();
        std::fs::remove_file(&a).ok();
        std::fs::remove_file(&b).ok();
        if !ok {
            eprintln!("skipped: {} cannot reflink", dir.display());
        }
        ok
    }

    fn spec(args: &[&str], env: &[&str]) -> TemplateSpec {
        serde_json::from_value(json!({ "args": args, "env": env })).unwrap()
    }

    #[test]
    fn boot_args_carry_the_program_and_the_fixed_guest_address() {
        let a = boot_args(
            &spec(&["python3", "-m", "http.server", "{{PORT}}"], &["A=1"]),
            8000,
            true,
        )
        .unwrap();
        assert!(a.contains("init=/sandstorm-init.sh"));
        assert!(a.contains("ip=10.42.0.2::10.42.0.1:255.255.255.252::eth0:off"));
        let hex = a.rsplit("sandstorm.app=").next().unwrap();
        let app: Value = serde_json::from_slice(&hex::decode(hex).unwrap()).unwrap();
        assert_eq!(app["args"][3], "8000", "{{PORT}} must be substituted");
        assert_eq!(app["env"][0], "A=1");
        assert!(!boot_args(&spec(&["x"], &[]), 1, false)
            .unwrap()
            .contains("ip="));
    }

    #[test]
    fn a_command_line_past_the_kernels_limit_is_refused() {
        let big = "x".repeat(1500);
        let e = boot_args(&spec(&["sh"], &[&big]), 1, true).unwrap_err();
        assert!(e.to_string().contains("2048"), "{e}");
    }

    #[test]
    fn a_chain_is_read_in_order_and_a_gap_is_refused() {
        let dir = TempDir::new().unwrap();
        for f in ["mem.0", "mem.2", "mem.1", "vmstate", ".mem.assembled"] {
            std::fs::write(dir.path().join(f), "").unwrap();
        }
        assert_eq!(chain_of(dir.path()).unwrap(), ["mem.0", "mem.1", "mem.2"]);
        std::fs::remove_file(dir.path().join("mem.1")).unwrap();
        assert!(chain_of(dir.path()).is_err());
    }

    /// A file of `len` bytes holding only `data`, the rest holes -- the shape
    /// of a Firecracker Diff snapshot.
    fn sparse(path: &Path, len: usize, data: &[(usize, Vec<u8>)]) {
        use std::io::{Seek, SeekFrom};
        let mut f = std::fs::File::create(path).unwrap();
        f.set_len(len as u64).unwrap();
        for (at, bytes) in data {
            f.seek(SeekFrom::Start(*at as u64)).unwrap();
            f.write_all(bytes).unwrap();
        }
    }

    /// No reflink needed, so this runs on CI's ext4: the SEEK_DATA walk must
    /// write a diff's data and leave the base alone under its holes --
    /// including a page of real zeros, which a hole-unaware copy would also
    /// write but a zero-scanning one would skip.
    #[test]
    fn a_diff_writes_its_data_and_nothing_under_its_holes() {
        const P: usize = 4096;
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("base");
        std::fs::write(&base, [vec![7u8; P], vec![7u8; P], vec![7u8; P]].concat()).unwrap();
        let diff = dir.path().join("diff");
        sparse(&diff, 3 * P, &[(0, vec![9u8; P]), (2 * P, vec![0u8; P])]);
        apply_diff(&diff, &base).unwrap();
        let got = std::fs::read(&base).unwrap();
        assert_eq!(&got[..P], &vec![9u8; P][..]);
        assert_eq!(
            &got[P..2 * P],
            &vec![7u8; P][..],
            "a hole must not be written"
        );
        #[cfg(target_os = "linux")]
        assert_eq!(
            &got[2 * P..],
            &vec![0u8; P][..],
            "zeros that are data are data"
        );
        let short = dir.path().join("short");
        sparse(&short, P, &[]);
        assert!(apply_diff(&short, &base).is_err());
    }

    /// Pages present in a later diff win; pages absent from it keep what
    /// the earlier links said.
    #[test]
    fn assembling_applies_each_diff_over_the_one_before() {
        const P: usize = 4096;
        let dir = TempDir::new().unwrap();
        let img = dir.path();
        if !reflinks(img) {
            return;
        }
        let page = |n: u8| vec![n; P];
        // mem.0: pages 1 1 1 1
        std::fs::write(
            img.join("mem.0"),
            [page(1), page(1), page(1), page(1)].concat(),
        )
        .unwrap();
        // mem.1: page 1 is 2, the rest are holes
        sparse(&img.join("mem.1"), 4 * P, &[(P, page(2))]);
        // mem.2: page 3 is 3
        sparse(&img.join("mem.2"), 4 * P, &[(3 * P, page(3))]);

        let chain = chain_of(img).unwrap();
        let out = assemble(img, &chain).unwrap();
        assert_eq!(out, img.join(".mem.assembled"));
        let got = std::fs::read(&out).unwrap();
        assert_eq!(got, [page(1), page(2), page(1), page(3)].concat());
        // Cached: a second fork maps the same file.
        assert_eq!(assemble(img, &chain).unwrap(), out);
        // A single full image is loaded as it is.
        let solo = dir.path().join("solo");
        std::fs::create_dir(&solo).unwrap();
        std::fs::write(solo.join("mem.0"), "m").unwrap();
        assert_eq!(
            assemble(&solo, &["mem.0".into()]).unwrap(),
            solo.join("mem.0")
        );
    }

    /// Images are whole on their own: linking a chain into a new image and
    /// then deleting the old one leaves the new one loadable.
    #[test]
    fn a_linked_chain_outlives_the_image_it_came_from() {
        let dir = TempDir::new().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::fs::create_dir(&a).unwrap();
        std::fs::write(a.join("mem.0"), "zero").unwrap();
        std::fs::write(a.join("mem.1"), "one").unwrap();
        std::fs::create_dir(&b).unwrap();
        std::fs::write(b.join("mem.0"), "stale").unwrap();
        link_chain(&a, &chain_of(&a).unwrap(), &b).unwrap();
        std::fs::remove_dir_all(&a).unwrap();
        assert_eq!(std::fs::read_to_string(b.join("mem.0")).unwrap(), "zero");
        assert_eq!(std::fs::read_to_string(b.join("mem.1")).unwrap(), "one");
    }

    #[test]
    fn prepare_records_what_later_calls_need() {
        let dir = TempDir::new().unwrap();
        if !reflinks(dir.path()) {
            return;
        }
        let fc = Firecracker {
            bin: "firecracker".into(),
            kernel: "/k".into(),
            vcpus: 2,
            work_dir: dir.path().to_path_buf(),
        };
        let tmpl = dir.path().join("rootfs.ext4");
        std::fs::write(&tmpl, "disk").unwrap();
        let s = spec(&["app"], &[]);
        let cfg = RuntimeConfig {
            work_dir: dir.path().to_path_buf(),
            ..Default::default()
        };
        let req = BuildRequest {
            id: "sb1",
            template_rootfs: &tmpl,
            spec: &s,
            port: 8000,
            volume: None,
            restore_from: None,
            netns: Some("/var/run/netns/sst-sb1"),
            sandbox_ip: Some(Ipv4Addr::new(10, 86, 0, 2)),
        };
        let b = fc.prepare(&cfg, &req).unwrap();
        assert_eq!(b, dir.path().join("bundles/sb1"));
        let got = fc.config("sb1").unwrap();
        assert_eq!(got.netns.as_deref(), Some("sst-sb1"));
        assert_eq!(got.sandbox_ip, Some(Ipv4Addr::new(10, 86, 0, 2)));
        assert_eq!((got.mem_mib, got.vcpus), (DEFAULT_MEM_MIB, 2));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("layers/sb1/disk/rootfs.ext4")).unwrap(),
            "disk"
        );
        // Again, as after a restart: the disk it has is kept.
        std::fs::write(dir.path().join("layers/sb1/disk/rootfs.ext4"), "written").unwrap();
        fc.prepare(&cfg, &req).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("layers/sb1/disk/rootfs.ext4")).unwrap(),
            "written"
        );
        let vol = BuildRequest {
            volume: Some("v"),
            ..req
        };
        assert!(fc.prepare(&cfg, &vol).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ficlone_is_the_kernels_number() {
        // _IOW(0x94, 9, int): write direction, a 4-byte argument.
        assert_eq!(FICLONE, (1 << 30) | (4 << 16) | (0x94 << 8) | 9);
    }
}
