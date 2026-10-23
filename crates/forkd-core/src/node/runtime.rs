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
//! `runsc create` + `runsc start` daemonises the sandbox, so it is not a
//! child of this process and a control-plane restart does not touch it.
//! The cgroup is named in the bundle's spec, so there is exactly one path
//! to write for a freeze.

use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::process::{Command as Exec, Stdio};
use std::time::{Duration, Instant};

use super::bundle::{self, TemplateSpec};
use crate::proto::ObservedSandbox;
use crate::tier::Tier;

/// Ceiling for a runsc command that should answer immediately. Generous
/// against a loaded node, short enough that a wedged runsc surfaces as an
/// error on this sandbox rather than a node that has stopped responding.
const RUNSC_TIMEOUT: Duration = Duration::from_secs(20);

/// Checkpoint and restore do real work proportional to the sandbox's memory.
/// Measured at 84 ms on bm, but a large sandbox on a loaded node is slower.
const RUNSC_SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(120);

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
            runsc_root: "/var/run/forkd-runsc".into(),
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

    /// For runsc commands that leave a long-lived process behind: create,
    /// start, restore.
    ///
    /// These must NOT go through `runsc()`. `Command::output()` reads the
    /// child's stdout and stderr until EOF, and the sandbox process inherits
    /// those pipes and keeps them open for as long as it runs -- so EOF
    /// never arrives and the call blocks for ever on a sandbox that started
    /// perfectly well. The symptom is a container stuck in `created` with no
    /// error anywhere. Give the sandbox a file for its output instead.
    fn runsc_spawn(&self, id: &str, args: &[&str]) -> Result<()> {
        let log_path = self.cfg.work_dir.join("logs");
        std::fs::create_dir_all(&log_path)?;
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path.join(format!("{id}.log")))
            .with_context(|| format!("opening the log for {id}"))?;

        let mut child = Exec::new(&self.cfg.runsc)
            .arg("--root")
            .arg(&self.cfg.runsc_root)
            .args(args)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .spawn()
            .with_context(|| format!("running {} {:?}", self.cfg.runsc, args))?;

        // Bounded for the same reason as `runsc`: a wedged runsc must not
        // take the node's loop with it.
        let deadline = Instant::now() + RUNSC_SNAPSHOT_TIMEOUT;
        let status = loop {
            match child.try_wait()? {
                Some(s) => break s,
                None if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    bail!(
                        "{} {:?} did not finish within {:?}",
                        self.cfg.runsc,
                        args,
                        RUNSC_SNAPSHOT_TIMEOUT
                    );
                }
                None => std::thread::sleep(Duration::from_millis(2)),
            }
        };
        if !status.success() {
            bail!(
                "{} {:?} exited with {status}; see {}",
                self.cfg.runsc,
                args,
                log_path.join(format!("{id}.log")).display()
            );
        }
        Ok(())
    }

    /// For short-lived runsc commands that answer and exit: list, delete,
    /// kill, checkpoint.
    ///
    /// Every one of these gets a deadline. runsc takes a lock on its state
    /// directory, and a wedged container makes even `runsc list` block --
    /// which blocks `observe()`, which blocks the node's whole loop. A node
    /// that stops polling because one container is stuck is worse than one
    /// that reports an error about it.
    ///
    /// Output goes to temporary files rather than pipes. `Command::output()`
    /// waits for EOF, and anything runsc leaves holding those descriptors
    /// keeps them open -- the same trap `runsc_spawn` documents.
    fn runsc(&self, args: &[&str]) -> Result<String> {
        self.runsc_timeout(args, RUNSC_TIMEOUT)
    }

    fn runsc_timeout(&self, args: &[&str], timeout: Duration) -> Result<String> {
        let tmp = self.cfg.work_dir.join("tmp");
        std::fs::create_dir_all(&tmp)?;
        let stem = format!("runsc-{}", std::process::id());
        let (out_path, err_path) = (
            tmp.join(format!("{stem}.out")),
            tmp.join(format!("{stem}.err")),
        );
        let out_file = std::fs::File::create(&out_path)?;
        let err_file = std::fs::File::create(&err_path)?;

        let mut child = Exec::new(&self.cfg.runsc)
            .arg("--root")
            .arg(&self.cfg.runsc_root)
            .args(args)
            .stdin(Stdio::null())
            .stdout(out_file)
            .stderr(err_file)
            .spawn()
            .with_context(|| format!("running {} {:?}", self.cfg.runsc, args))?;

        let deadline = Instant::now() + timeout;
        let status = loop {
            match child.try_wait()? {
                Some(s) => break s,
                None if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    bail!(
                        "{} {:?} did not finish within {:?}; runsc may be wedged on \
                         its state directory",
                        self.cfg.runsc,
                        args,
                        timeout
                    );
                }
                None => std::thread::sleep(Duration::from_millis(2)),
            }
        };

        let stdout = std::fs::read_to_string(&out_path).unwrap_or_default();
        let stderr = std::fs::read_to_string(&err_path).unwrap_or_default();
        std::fs::remove_file(&out_path).ok();
        std::fs::remove_file(&err_path).ok();

        if !status.success() {
            bail!("{} {:?} failed: {}", self.cfg.runsc, args, stderr.trim());
        }
        Ok(stdout)
    }

    /// What this node actually holds right now, read from the filesystem
    /// and cgroups. No subprocess, and in particular no `runsc list`.
    ///
    /// `runsc list` queries each sandbox's sentry over its socket, and a
    /// frozen sentry cannot answer: measured on bm, `runsc list` times out
    /// while any sandbox is frozen and succeeds the instant it is thawed.
    /// Since freezing is the whole point of the frozen tier, asking runsc
    /// would mean one frozen sandbox blinds the node to every other -- the
    /// node stops polling, and nothing recovers until someone thaws it by
    /// hand. So state comes from places that cannot block:
    ///
    ///   snapshots/<id> exists            -> Snapshot
    ///   cgroup exists and has processes  -> Frozen or Hot, per cgroup.freeze
    ///   neither                          -> not here; the master will Create
    ///
    /// Enumerating our own bundles directory also means we can only ever
    /// see our own sandboxes, which is stronger than filtering someone
    /// else's output: anything we report that desired state does not list
    /// gets destroyed.
    pub fn observe(&self) -> Result<Vec<ObservedSandbox>> {
        let mut found = Vec::new();
        let bundles = self.cfg.work_dir.join("bundles");
        let rd = match std::fs::read_dir(&bundles) {
            Ok(rd) => rd,
            // Nothing created yet: not an error, just an empty node.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(found),
            Err(e) => return Err(e).context("listing bundles"),
        };

        for entry in rd.flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            let id = entry.file_name().to_string_lossy().into_owned();

            if self.cfg.work_dir.join("snapshots").join(&id).is_dir() {
                found.push(ObservedSandbox {
                    id,
                    tier: Tier::Snapshot,
                    serving: false,
                });
                continue;
            }

            let cg = self.cgroup_path(&id);
            let running = std::fs::read_to_string(cg.join("cgroup.procs"))
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false);
            if !running {
                // The bundle is here but nothing is in its cgroup: a failed
                // start, or a sandbox that died. Reporting it as present
                // would stop the master retrying it.
                continue;
            }
            let frozen = self.is_frozen(&id);
            found.push(ObservedSandbox {
                id,
                tier: if frozen { Tier::Frozen } else { Tier::Hot },
                serving: !frozen,
            });
        }
        Ok(found)
    }

    fn cgroup_path(&self, id: &str) -> PathBuf {
        PathBuf::from("/sys/fs/cgroup/forkd").join(id)
    }

    fn is_frozen(&self, id: &str) -> bool {
        std::fs::read_to_string(self.cgroup_path(id).join("cgroup.freeze"))
            .map(|s| s.trim() == "1")
            .unwrap_or(false)
    }

    /// Memory the sandbox is actually holding, for the density accounting.
    pub fn memory_mib(&self, id: &str) -> Option<u64> {
        std::fs::read_to_string(self.cgroup_path(id).join("memory.current"))
            .ok()?
            .trim()
            .parse::<u64>()
            .ok()
            .map(|b| b / 1048576)
    }

    /// Bring a sandbox up from nothing.
    ///
    /// The template is resolved on the node, not passed down from the
    /// master: the rootfs is a local path, and a master should not be able
    /// to name an arbitrary directory on a node's disk.
    pub fn create(
        &self,
        id: &str,
        template: &str,
        spec: &serde_json::Value,
        port: u16,
        volume: Option<&str>,
    ) -> Result<()> {
        let rootfs = self.template_rootfs(template)?;
        let spec: TemplateSpec = serde_json::from_value(spec.clone())
            .with_context(|| format!("template {template} has an unusable spec"))?;

        // A sandbox that was detached has its layer archived on the volume,
        // and "Create" is how it comes back: nothing on this node observes a
        // detached sandbox, and a cold start needs the template and port
        // anyway. The archive goes down before the overlay is mounted.
        let archive = self.archive_path(id);
        let restore_from = archive.exists().then_some(archive.as_path());
        if restore_from.is_some() {
            tracing::info!(%id, archive = %archive.display(), "restoring detached layer");
        }

        // The master allocates ports without knowing what is bound on this
        // node, so check before starting. Otherwise the only symptom is the
        // sandbox's own traceback in its log, which cost real debugging time
        // when an orphaned sandbox was still holding a port.
        if port != 0 {
            if let Err(e) = std::net::TcpListener::bind(("127.0.0.1", port)) {
                bail!("port {port} is not free on this node: {e}");
            }
        }

        let layout = bundle::build(&self.cfg, id, &rootfs, &spec, port, volume, restore_from)
            .with_context(|| format!("building bundle for {id}"))?;

        tracing::info!(%id, %template, port, ?volume, "create");

        // create then start, rather than `run`: create reports a bad bundle
        // as an error we can attribute, and start is what the restore path
        // needs to look like too.
        self.runsc_spawn(
            id,
            &[
                "--network=host",
                "create",
                "--bundle",
                &layout.bundle.to_string_lossy(),
                id,
            ],
        )
        .with_context(|| format!("runsc create {id}"))?;
        self.runsc_spawn(id, &["--network=host", "start", id])
            .with_context(|| format!("runsc start {id}"))?;
        tracing::info!(%id, mem_mib = ?self.memory_mib(id), "running");
        Ok(())
    }

    fn archive_path(&self, id: &str) -> PathBuf {
        self.cfg
            .volume_root
            .join(id)
            .join("_archive")
            .join("layer.tar.zst")
    }

    /// Where a template's rootfs lives on this node.
    ///
    /// The name becomes a path component, so it must be a plain name: a
    /// master that could send `../..` here would be choosing a directory on
    /// the node's disk.
    fn template_rootfs(&self, name: &str) -> Result<PathBuf> {
        if name.is_empty() {
            bail!("no template named");
        }
        if name.contains('/') || name.contains('\\') || name.contains("..") || name.starts_with('.')
        {
            bail!("template name {name:?} is not a plain name");
        }
        let dir = self
            .cfg
            .work_dir
            .join("templates")
            .join(name)
            .join("rootfs");
        if !dir.is_dir() {
            bail!(
                "template {name} is not installed on this node (no {})",
                dir.display()
            );
        }
        Ok(dir)
    }

    /// Move a sandbox to a tier, walking the intermediate legs if the
    /// master asked for a jump. Each leg is cheap; doing them one at a time
    /// is what keeps a demotion from stalling on a tar.
    pub fn set_tier(&self, id: &str, want: Tier) -> Result<()> {
        for _ in 0..Tier::ORDER.len() {
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
            // Colder: one step. Warmer: straight back to hot.
            let next = if want > have {
                have.colder().unwrap_or(want)
            } else {
                Tier::Hot
            };
            self.step(id, have, next)
                .with_context(|| format!("{id}: {have} -> {next}"))?;
            if next == want {
                return Ok(());
            }
        }
        bail!("{id} did not reach {want}")
    }

    fn step(&self, id: &str, have: Tier, want: Tier) -> Result<()> {
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
            // Nothing on this node observes a detached sandbox, so the
            // master issues Create for it instead; create() notices the
            // archive on the volume and unpacks it.
            (Tier::Detached, Tier::Hot) => bail!(
                "{id} is detached; it comes back via Create, which carries \
                 the template and port a cold start needs"
            ),
            (from, to) => bail!("no {from} -> {to} transition"),
        }
    }

    fn freeze(&self, id: &str, on: bool) -> Result<()> {
        let cg = self.cgroup_path(id);
        let p = cg.join("cgroup.freeze");
        std::fs::write(&p, if on { "1" } else { "0" })
            .with_context(|| format!("writing {}", p.display()))?;

        // The write returns before the tasks have settled, so wait for the
        // kernel to say so. Measured at 8-11 ms; a second is a generous
        // ceiling that still fails rather than hanging for ever.
        let want = if on { "frozen 1" } else { "frozen 0" };
        let events = cg.join("cgroup.events");
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if std::fs::read_to_string(&events)
                .map(|s| s.lines().any(|l| l.trim() == want))
                .unwrap_or(false)
            {
                break;
            }
            if Instant::now() > deadline {
                bail!("{id} did not report {want} within 1s");
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        tracing::info!(%id, frozen = on, "freeze");
        Ok(())
    }

    fn checkpoint(&self, id: &str) -> Result<()> {
        let dir = self.cfg.work_dir.join("snapshots").join(id);
        std::fs::create_dir_all(&dir)?;
        let img = dir.join("checkpoint.img");
        self.runsc_timeout(
            &[
                "--network=host",
                "checkpoint",
                &format!("--image-path={}", dir.display()),
                id,
            ],
            RUNSC_SNAPSHOT_TIMEOUT,
        )
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
        if !bundle.join("config.json").exists() {
            bail!(
                "no bundle for {id} at {}; its layer is gone, so this is a \
                 detached sandbox and must be attached rather than restored",
                bundle.display()
            );
        }
        self.runsc_spawn(
            id,
            &[
                "--network=host",
                "restore",
                "--bundle",
                &bundle.to_string_lossy(),
                &format!("--image-path={}", dir.display()),
                "--detach",
                id,
            ],
        )
        .with_context(|| format!("restoring {id}"))?;
        std::fs::remove_dir_all(&dir).ok();
        tracing::info!(%id, "restored");
        Ok(())
    }

    /// Everything local goes into one archive on the volume. Measured on bm
    /// with 30k files: one tar+zstd round trip is 16.1 s against 57 s for
    /// per-file copies onto the volume, because the volume pays per file.
    fn detach(&self, id: &str) -> Result<()> {
        // Unmount first, then archive: by here the sandbox is checkpointed
        // and gone, so nothing needs the overlay, and reading a live
        // upperdir is best avoided.
        bundle::unmount(&self.cfg, id)?;

        // Only the overlay upper dir: the lower is the shared template and
        // would be the same bytes for every sandbox on the node.
        let layer = self.cfg.work_dir.join("layers").join(id).join("upper");
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
        std::fs::remove_dir_all(self.cfg.work_dir.join("snapshots").join(id)).ok();
        // Unmount and drop the layer: detached means nothing local at all.
        bundle::teardown(&self.cfg, id)?;
        tracing::info!(%id, archive = %arc.display(), "detached");
        Ok(())
    }

    pub fn destroy(&self, id: &str) -> Result<()> {
        // Thaw first: a frozen cgroup's tasks cannot be killed at all. On bm
        // this left gvisor_sentry processes that nothing could reap and that
        // went on holding their sandbox's port.
        let _ = self.freeze(id, false);
        let _ = self.runsc(&["kill", id, "KILL"]);
        let _ = self.runsc(&["delete", "-force", id]);

        // Backstop: runsc's own kill path can leave the sentry running --
        // certainly when runsc's state for the container is already gone.
        // cgroup.kill takes the whole cgroup regardless of what runsc thinks.
        let cg = self.cgroup_path(id);
        if cg.exists() {
            if std::fs::write(cg.join("cgroup.kill"), "1").is_ok() {
                // The kill is asynchronous; wait for the cgroup to empty so
                // the port is really free before we report success.
                let deadline = Instant::now() + Duration::from_secs(5);
                while Instant::now() < deadline {
                    let empty = std::fs::read_to_string(cg.join("cgroup.procs"))
                        .map(|s| s.trim().is_empty())
                        .unwrap_or(true);
                    if empty {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            std::fs::remove_dir(&cg).ok();
        }
        std::fs::remove_dir_all(self.cfg.work_dir.join("snapshots").join(id)).ok();
        bundle::teardown(&self.cfg, id)?;
        // The archived layer is ours; the volume's /workspace is the user's
        // and is left alone.
        if let Some(d) = self.archive_path(id).parent() {
            std::fs::remove_dir_all(d).ok();
        }
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

#[cfg(test)]
mod observe_tests {
    use super::*;
    use tempfile::TempDir;

    fn rt(work: &std::path::Path) -> Runtime {
        Runtime {
            cfg: RuntimeConfig {
                work_dir: work.to_path_buf(),
                volume_root: work.join("volumes"),
                ..Default::default()
            },
        }
    }

    #[test]
    fn an_empty_node_observes_nothing() {
        let td = TempDir::new().unwrap();
        assert!(rt(td.path()).observe().unwrap().is_empty());
    }

    /// A bundle with nothing in its cgroup is a failed start, not a running
    /// sandbox. Reporting it as present would stop the master retrying it.
    #[test]
    fn a_bundle_without_processes_is_not_reported() {
        let td = TempDir::new().unwrap();
        std::fs::create_dir_all(td.path().join("bundles/sb1")).unwrap();
        assert!(rt(td.path()).observe().unwrap().is_empty());
    }

    /// Snapshot is read from the filesystem, so it needs no runsc and no
    /// cgroup -- which is what makes it observable at all.
    #[test]
    fn a_checkpointed_sandbox_is_observed_as_snapshot() {
        let td = TempDir::new().unwrap();
        std::fs::create_dir_all(td.path().join("bundles/sb1")).unwrap();
        std::fs::create_dir_all(td.path().join("snapshots/sb1")).unwrap();
        let got = rt(td.path()).observe().unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].tier, Tier::Snapshot);
        assert!(!got[0].serving);
    }

    /// A detached sandbox has no bundle, so it is not on this node at all.
    #[test]
    fn a_detached_sandbox_is_not_on_this_node() {
        let td = TempDir::new().unwrap();
        std::fs::create_dir_all(td.path().join("volumes/sb1/_archive")).unwrap();
        assert!(rt(td.path()).observe().unwrap().is_empty());
    }

    #[test]
    fn only_our_own_bundles_can_ever_be_seen() {
        let td = TempDir::new().unwrap();
        // Another deployment's bundle, in a sibling directory.
        std::fs::create_dir_all(td.path().join("../other/bundles/theirs")).ok();
        std::fs::create_dir_all(td.path().join("bundles/ours")).unwrap();
        std::fs::create_dir_all(td.path().join("snapshots/ours")).unwrap();
        let got = rt(td.path()).observe().unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "ours");
    }
}
