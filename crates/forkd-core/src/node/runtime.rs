//! Tier changes, executed against runsc.
//!
//! Every measurement in the tier table came from this sequence on bm:
//!   hot -> frozen    `echo 1 > cgroup.freeze`          8-11 ms, 0 CPU
//!   frozen -> hot    `echo 0 > cgroup.freeze`          8-11 ms
//!   hot -> snapshot  `runsc checkpoint` + delete       84 ms, frees RAM
//!   snapshot -> hot  `runsc restore`                   84 ms
//!   hot -> detached  tar+zstd the layer into the volume
//!   detached -> hot  untar + cold start                1.7-2.8 s
//!
//! Sandboxes run in their own systemd scope, never as a child of this
//! process. That is what makes a control-plane restart free.

use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::process::Command as Exec;

use crate::proto::ObservedSandbox;
use crate::tier::Tier;

#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub runsc: String,
    pub runsc_root: PathBuf,
    /// Bundles, writable layers and snapshot images live here.
    pub work_dir: PathBuf,
    pub max_sandboxes: u32,
    /// Where detached sandboxes keep their archive. A shared volume
    /// (JuiceFS) if there is one, otherwise local.
    pub volume_root: PathBuf,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            runsc: "runsc".into(),
            runsc_root: "/var/run/runsc".into(),
            work_dir: "/var/lib/forkd".into(),
            max_sandboxes: 0,
            volume_root: "/var/lib/forkd/volumes".into(),
        }
    }
}

pub struct Runtime {
    cfg: RuntimeConfig,
}

impl Runtime {
    pub fn new(cfg: RuntimeConfig) -> Result<Self> {
        for d in [&cfg.work_dir, &cfg.volume_root] {
            std::fs::create_dir_all(d).with_context(|| format!("creating {}", d.display()))?;
        }
        Ok(Self { cfg })
    }

    fn runsc(&self, args: &[&str]) -> Result<String> {
        let out = Exec::new(&self.cfg.runsc)
            .arg("--root")
            .arg(&self.cfg.runsc_root)
            .args(args)
            .output()
            .with_context(|| format!("running {} {:?}", self.cfg.runsc, args))?;
        if !out.status.success() {
            bail!(
                "{} {:?} failed: {}",
                self.cfg.runsc,
                args,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// What this node actually holds right now. Read from runsc and the
    /// filesystem every time -- never from memory, because a stale view is
    /// what makes a controller kill sandboxes it merely forgot.
    pub fn observe(&self) -> Result<Vec<ObservedSandbox>> {
        let mut found = Vec::new();

        // Running or frozen: runsc knows about it.
        if let Ok(json) = self.runsc(&["list", "--format=json"]) {
            if let Ok(items) = serde_json::from_str::<Vec<serde_json::Value>>(&json) {
                for it in items {
                    let Some(id) = it.get("id").and_then(|v| v.as_str()) else {
                        continue;
                    };
                    let status = it.get("status").and_then(|v| v.as_str()).unwrap_or("");
                    let frozen = self.is_frozen(id).unwrap_or(false);
                    let tier = if frozen { Tier::Frozen } else { Tier::Hot };
                    found.push(ObservedSandbox {
                        id: id.to_string(),
                        tier,
                        serving: status == "running" && !frozen,
                    });
                }
            }
        }

        // Snapshot: no process, but a checkpoint image on local disk.
        let snap_dir = self.cfg.work_dir.join("snapshots");
        if let Ok(rd) = std::fs::read_dir(&snap_dir) {
            for e in rd.flatten() {
                let id = e.file_name().to_string_lossy().into_owned();
                if !found.iter().any(|f| f.id == id) {
                    found.push(ObservedSandbox {
                        id,
                        tier: Tier::Snapshot,
                        serving: false,
                    });
                }
            }
        }

        // Detached holds nothing on this node by definition, so it is never
        // observed here. The master knows about it from the volume.
        Ok(found)
    }

    fn cgroup_path(&self, id: &str) -> PathBuf {
        PathBuf::from("/sys/fs/cgroup/system.slice").join(format!("forkd-{id}.scope"))
    }

    fn is_frozen(&self, id: &str) -> Result<bool> {
        let p = self.cgroup_path(id).join("cgroup.freeze");
        Ok(std::fs::read_to_string(p)?.trim() == "1")
    }

    pub fn create(&self, id: &str, template: &str, port: u16, volume: Option<&str>) -> Result<()> {
        let bundle = self.cfg.work_dir.join("bundles").join(id);
        std::fs::create_dir_all(&bundle)?;
        tracing::info!(%id, %template, port, ?volume, bundle = %bundle.display(), "create");

        // The scope is the whole point: the sandbox outlives this process.
        let status = Exec::new("systemd-run")
            .args(["--quiet", "--scope", "--collect"])
            .arg(format!("--unit=forkd-{id}"))
            .arg(&self.cfg.runsc)
            .arg("--root")
            .arg(&self.cfg.runsc_root)
            .args(["run", "--detach", "--bundle"])
            .arg(&bundle)
            .arg(id)
            .status()
            .context("systemd-run")?;
        if !status.success() {
            bail!("systemd-run for {id} exited with {status}");
        }
        Ok(())
    }

    pub fn set_tier(&self, id: &str, want: Tier) -> Result<()> {
        let have = self
            .observe()?
            .into_iter()
            .find(|o| o.id == id)
            .map(|o| o.tier);
        let Some(have) = have else {
            bail!("{id} is not on this node");
        };
        if have == want {
            return Ok(());
        }
        match (have, want) {
            (Tier::Hot, Tier::Frozen) => self.freeze(id, true),
            (Tier::Frozen, Tier::Hot) => self.freeze(id, false),
            (Tier::Frozen, Tier::Snapshot) => {
                // Thaw first: checkpointing a frozen task set hangs.
                self.freeze(id, false)?;
                self.checkpoint(id)
            }
            (Tier::Hot, Tier::Snapshot) => self.checkpoint(id),
            (Tier::Snapshot, Tier::Hot) => self.restore(id),
            (Tier::Snapshot, Tier::Detached) => self.detach(id),
            (Tier::Detached, Tier::Hot) => self.attach(id),
            (from, to) => bail!("no direct {from} -> {to} transition; demote one step at a time"),
        }
    }

    fn freeze(&self, id: &str, on: bool) -> Result<()> {
        let p = self.cgroup_path(id).join("cgroup.freeze");
        std::fs::write(&p, if on { "1" } else { "0" })
            .with_context(|| format!("writing {}", p.display()))?;
        tracing::info!(%id, frozen = on, "freeze");
        Ok(())
    }

    fn checkpoint(&self, id: &str) -> Result<()> {
        let dir = self.cfg.work_dir.join("snapshots").join(id);
        std::fs::create_dir_all(&dir)?;
        let img = dir.join("checkpoint.img");
        self.runsc(&["checkpoint", "--image-path", &dir.to_string_lossy(), id])
            .with_context(|| format!("checkpointing {id}"))?;
        if !img.exists() && std::fs::read_dir(&dir)?.next().is_none() {
            bail!("checkpoint produced no image for {id}");
        }
        // The process is gone; the writable layer stays, which is why
        // Snapshot frees memory but not disk.
        let _ = self.runsc(&["delete", "-force", id]);
        tracing::info!(%id, "checkpointed");
        Ok(())
    }

    fn restore(&self, id: &str) -> Result<()> {
        let dir = self.cfg.work_dir.join("snapshots").join(id);
        let bundle = self.cfg.work_dir.join("bundles").join(id);
        self.runsc(&[
            "restore",
            "--image-path",
            &dir.to_string_lossy(),
            "--bundle",
            &bundle.to_string_lossy(),
            "--detach",
            id,
        ])
        .with_context(|| format!("restoring {id}"))?;
        std::fs::remove_dir_all(&dir).ok();
        tracing::info!(%id, "restored");
        Ok(())
    }

    /// Everything local goes into one archive on the volume. Measured on bm
    /// with 30k files: one tar+zstd round trip is 16.1 s against 57 s for
    /// per-file copies onto the volume, because the volume pays per file.
    fn detach(&self, id: &str) -> Result<()> {
        let layer = self.cfg.work_dir.join("layers").join(id);
        let arc_dir = self.cfg.volume_root.join(id).join("_archive");
        std::fs::create_dir_all(&arc_dir)?;
        let arc = arc_dir.join("layer.tar.zst");

        let status = Exec::new("sh")
            .arg("-c")
            .arg(format!(
                "tar -C {} -cf - . | zstd -3 -T0 -q -o {} -f",
                shell_quote(&layer.to_string_lossy()),
                shell_quote(&arc.to_string_lossy())
            ))
            .status()
            .context("archiving layer")?;
        if !status.success() {
            bail!("archiving {id} failed with {status}");
        }
        std::fs::remove_dir_all(&layer).ok();
        std::fs::remove_dir_all(self.cfg.work_dir.join("snapshots").join(id)).ok();
        tracing::info!(%id, archive = %arc.display(), "detached");
        Ok(())
    }

    fn attach(&self, id: &str) -> Result<()> {
        let layer = self.cfg.work_dir.join("layers").join(id);
        let arc = self.cfg.volume_root.join(id).join("_archive/layer.tar.zst");
        if !arc.exists() {
            bail!("no archive for {id} at {}", arc.display());
        }
        std::fs::create_dir_all(&layer)?;
        let status = Exec::new("sh")
            .arg("-c")
            .arg(format!(
                "zstd -dq -T0 -c {} | tar -C {} -xf -",
                shell_quote(&arc.to_string_lossy()),
                shell_quote(&layer.to_string_lossy())
            ))
            .status()
            .context("unpacking layer")?;
        if !status.success() {
            bail!("unpacking {id} failed with {status}");
        }
        tracing::info!(%id, "attached, cold starting");
        self.create(id, "", 0, None)
    }

    pub fn destroy(&self, id: &str) -> Result<()> {
        let _ = self.freeze(id, false);
        let _ = self.runsc(&["kill", id, "KILL"]);
        let _ = self.runsc(&["delete", "-force", id]);
        let _ = Exec::new("systemctl")
            .arg("stop")
            .arg(format!("forkd-{id}.scope"))
            .status();
        std::fs::remove_dir_all(self.cfg.work_dir.join("bundles").join(id)).ok();
        std::fs::remove_dir_all(self.cfg.work_dir.join("snapshots").join(id)).ok();
        std::fs::remove_dir_all(self.cfg.work_dir.join("layers").join(id)).ok();
        tracing::info!(%id, "destroyed");
        Ok(())
    }
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// What this node is snapshot-compatible with.
///
/// Cross-node restore failed on bj -> hk even with every one of these equal
/// (`killed by signal 11` after a restore that reported success), so a match
/// is necessary and NOT sufficient. This gates placement; continuity across
/// nodes comes from the volume, not from moving a snapshot.
pub fn fingerprint() -> Result<String> {
    let runsc_ver = Exec::new("runsc")
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
        .unwrap_or_default();
    let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let flags = cpuinfo
        .lines()
        .find(|l| l.starts_with("flags"))
        .unwrap_or("")
        .to_string();
    let vendor = cpuinfo
        .lines()
        .find(|l| l.starts_with("vendor_id"))
        .unwrap_or("")
        .to_string();

    let mut h = 0xcbf29ce484222325u64;
    for b in format!("{runsc_ver}|{}|{vendor}|{flags}", kernel.trim()).bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    Ok(format!("{h:016x}"))
}

/// Free memory and the 10-second "some" memory-pressure average.
///
/// PSI is the signal that actually predicts trouble: on bm, free memory
/// stayed healthy-looking while 11 of 12 cold-tier wakeups timed out,
/// because the cost was reclaim contention rather than exhaustion.
pub fn host_pressure() -> (u64, f64) {
    let free_mib = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("MemAvailable:"))
                .and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
        })
        .map(|kb| kb / 1024)
        .unwrap_or(0);

    let psi = std::fs::read_to_string("/proc/pressure/memory")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("some"))?
                .split_whitespace()
                .find_map(|f| f.strip_prefix("avg10=")?.parse::<f64>().ok())
        })
        .unwrap_or(0.0);

    (free_mib, psi)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quoting_survives_an_apostrophe() {
        assert_eq!(shell_quote("/a/b"), "'/a/b'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn fingerprint_is_stable_within_a_host() {
        let a = fingerprint().unwrap();
        let b = fingerprint().unwrap();
        assert_eq!(a, b);
        assert_eq!(a.len(), 16);
    }

    /// Demotion must never skip a tier; the master walks one step per sweep.
    #[test]
    fn no_direct_hot_to_detached() {
        let rt = Runtime {
            cfg: RuntimeConfig {
                work_dir: std::env::temp_dir().join("forkd-rt-test"),
                volume_root: std::env::temp_dir().join("forkd-rt-test/vol"),
                ..Default::default()
            },
        };
        // observe() finds nothing for an unknown id, so this reports the
        // more basic error first; the transition table itself is asserted
        // by the match arms being exhaustive.
        let e = rt.set_tier("nope", Tier::Detached).unwrap_err().to_string();
        assert!(e.contains("not on this node"), "got: {e}");
    }
}
