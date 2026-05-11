//! `forkd-vmm`: Firecracker wrapper with snapshot/fork primitives.
//!
//! See `DESIGN.md` at the repo root for the architecture.
//!
//! HTTP-over-unix-socket is currently implemented by shelling out to `curl`.
//! This avoids pulling in a heavy HTTP client for the MVP. It's slow per call
//! (~10–20 ms startup) but we issue calls in parallel via threads, so the
//! aggregate wall-clock is dominated by Firecracker, not curl.
//! A future PR can replace curl with hyper + hyperlocal.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BootConfig {
    pub kernel: PathBuf,
    pub rootfs: PathBuf,
    pub vcpu_count: u32,
    pub mem_size_mib: u32,
    pub boot_args: String,
    pub work_dir: PathBuf,
}

impl BootConfig {
    /// Sensible defaults for a Firecracker-quickstart-style boot:
    /// 2 vCPU, 512 MiB, ttyS0 console, read-only rootfs on /dev/vda.
    pub fn quickstart(kernel: PathBuf, rootfs: PathBuf, work_dir: PathBuf) -> Self {
        Self {
            kernel,
            rootfs,
            vcpu_count: 2,
            mem_size_mib: 512,
            boot_args: "console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda ro".into(),
            work_dir,
        }
    }
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
}

/// Result of `Snapshot::restore_many` — N live children plus timing.
#[derive(Debug)]
pub struct ForkResult {
    pub children: Vec<Vm>,
    pub spawn_ms: u128,
    pub restore_ms: u128,
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

/// Default per-call timeout. Most Firecracker API calls return in <1 ms.
const DEFAULT_API_TIMEOUT_SECS: u32 = 10;
/// Snapshot create writes the full memory image to disk and is I/O bound.
/// 512 MiB measured at ~3.3 s on our dev box; allow 60 s for slower disks.
const SNAPSHOT_TIMEOUT_SECS: u32 = 60;

fn api_call(sock: &Path, method: &str, path: &str, body: &str) -> Result<()> {
    api_call_with_timeout(sock, method, path, body, DEFAULT_API_TIMEOUT_SECS)
}

fn api_call_with_timeout(
    sock: &Path,
    method: &str,
    path: &str,
    body: &str,
    timeout_secs: u32,
) -> Result<()> {
    let url = format!("http://localhost{path}");
    let timeout = timeout_secs.to_string();
    let out = Command::new("curl")
        .args(["-sS", "--max-time", &timeout, "--unix-socket"])
        .arg(sock)
        .args(["-X", method, &url])
        .args(["-H", "Content-Type: application/json", "-d", body])
        .output()
        .context("failed to spawn curl")?;
    if !out.status.success() {
        bail!(
            "firecracker API {} {} failed: {}",
            method,
            path,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
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
    let f = std::fs::File::create(console).context("create console log file")?;
    let f_err = f.try_clone()?;
    Command::new("firecracker")
        .arg("--api-sock")
        .arg(sock)
        .stdin(Stdio::null())
        .stdout(f)
        .stderr(f_err)
        .spawn()
        .context("failed to spawn firecracker")
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

        let body = serde_json::json!({
            "kernel_image_path": cfg.kernel,
            "boot_args": cfg.boot_args,
        });
        api_call(&sock, "PUT", "/boot-source", &body.to_string())?;

        let body = serde_json::json!({
            "drive_id": "rootfs",
            "path_on_host": cfg.rootfs,
            "is_root_device": true,
            "is_read_only": true,
        });
        api_call(&sock, "PUT", "/drives/rootfs", &body.to_string())?;

        let body = serde_json::json!({
            "vcpu_count": cfg.vcpu_count,
            "mem_size_mib": cfg.mem_size_mib,
            "track_dirty_pages": true,
        });
        api_call(&sock, "PUT", "/machine-config", &body.to_string())?;

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
        })
    }

    /// Pause the VM (no vCPU progress). Required before snapshot.
    pub fn pause(&self) -> Result<()> {
        api_call(&self.sock, "PATCH", "/vm", r#"{"state":"Paused"}"#)
    }

    /// Write a Full snapshot to disk. VM must be paused first.
    pub fn snapshot_to(&self, vmstate: PathBuf, memory: PathBuf) -> Result<Snapshot> {
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
        Ok(Snapshot { vmstate, memory })
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
        std::fs::create_dir_all(work_dir).context("create fork work_dir")?;
        for entry in std::fs::read_dir(work_dir)? {
            let p = entry?.path();
            if p.is_file() {
                let _ = std::fs::remove_file(&p);
            }
        }

        // Phase 1: spawn N firecracker processes, wait for sockets.
        let spawn_start = Instant::now();
        let mut children: Vec<Vm> = Vec::with_capacity(n);
        for i in 1..=n {
            let sock = work_dir.join(format!("child-{i}.sock"));
            let console = work_dir.join(format!("child-{i}.console"));
            let proc = spawn_firecracker(&sock, &console)?;
            let pid = proc.id();
            children.push(Vm {
                proc,
                pid,
                sock,
                console,
            });
        }
        for c in &children {
            wait_for_sock(&c.sock, Duration::from_secs(5))?;
        }
        let spawn_ms = spawn_start.elapsed().as_millis();

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

        Ok(ForkResult {
            children,
            spawn_ms,
            restore_ms,
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
    }

    #[test]
    fn snapshot_serializes_round_trip() {
        let s = Snapshot {
            vmstate: "/tmp/v".into(),
            memory: "/tmp/m".into(),
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: Snapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(s.vmstate, back.vmstate);
        assert_eq!(s.memory, back.memory);
    }
}
