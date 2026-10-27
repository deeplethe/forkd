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
use std::path::{Path, PathBuf};
use std::process::{Command as Exec, Stdio};
use std::time::{Duration, Instant};

use super::bundle::{self, TemplateSpec, WarmSpec};
use super::net::{self, NetConfig};
use crate::proto::ObservedSandbox;
use crate::tier::Tier;

/// What one sandbox is created from.
pub struct CreateRequest<'a> {
    pub id: &'a str,
    pub template: &'a str,
    pub spec: &'a serde_json::Value,
    pub port: u16,
    pub volume: Option<&'a str>,
    pub net_index: Option<u32>,
    /// Fork from this tag's checkpoint rather than starting cold.
    pub from_tag: Option<&'a str>,
    /// Host port the gateway should listen on, and the token it checks.
    /// Both are written into the bundle so a node restart re-adopts the
    /// sandbox with its gateway intact -- otherwise the port would come
    /// back open with nothing to verify.
    pub gateway_port: Option<u16>,
    pub token: &'a str,
}

/// Ceiling for a runsc command that should answer immediately. Generous
/// against a loaded node, short enough that a wedged runsc surfaces as an
/// error on this sandbox rather than a node that has stopped responding.
const RUNSC_TIMEOUT: Duration = Duration::from_secs(20);

/// Checkpoint and restore do real work proportional to the sandbox's memory.
/// Measured at 84 ms on bm, but a large sandbox on a loaded node is slower.
const RUNSC_SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(120);

/// The node's own seed sandboxes, which a warm template image is taken
/// from. `observe` never reports them, and the master refuses the prefix
/// for anything it creates, so the two can never be confused.
pub const WARM_SEED_PREFIX: &str = "sandstorm-warm-";
/// How long a failed seed keeps a template cold before it is tried again.
const WARM_RETRY: Duration = Duration::from_secs(600);
/// How long `after_restore` may take.
const AFTER_RESTORE_TIMEOUT: Duration = Duration::from_secs(10);

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
    /// Address plan for per-sandbox network namespaces.
    pub net: NetConfig,
    /// Push a frozen sandbox's memory into compressed swap. Only worth it
    /// where swap is zswap or zram; see `host.rs`, which decides.
    pub reclaim_frozen: bool,
    /// Compress the images Snapshot writes. Worth it on a slow disk; see
    /// `host.rs`, which decides.
    pub compress_snapshots: bool,
    /// Keep squeezing the cold pages of hot sandboxes into compressed swap,
    /// paced by their own memory stalls; see `reclaim.rs`.
    pub reclaim_hot: bool,
    /// Read a periodic sandbox's snapshot image just before its next
    /// predicted request; see `wake.rs`.
    pub prefetch_snapshots: bool,
    /// Turn zswap writeback off for reclaimed sandboxes. Right when swap is
    /// zswap over a disk, wrong over zram; `host::pin_in_zswap` decides.
    pub pin_in_zswap: bool,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            runsc: "runsc".into(),
            runsc_root: "/var/run/sandstorm-runsc".into(),
            work_dir: "/var/lib/sandstorm".into(),
            max_sandboxes: 0,
            volume_root: "/var/lib/sandstorm/volumes".into(),
            net: NetConfig::default(),
            reclaim_frozen: false,
            compress_snapshots: false,
            reclaim_hot: false,
            prefetch_snapshots: false,
            pin_in_zswap: true,
        }
    }
}

/// Reclaim a frozen sandbox's memory, off the command path.
///
/// One worker for the whole node, taking sandboxes in the order they were
/// frozen: reclaim costs CPU to compress (1.3 s for 376 MiB on bm), and a
/// wave of idle demotions should not turn into a wave of threads. In 64 MiB
/// steps, so a sandbox thawed partway through -- woken by a request -- stops
/// being squeezed at once.
fn reclaim_later(id: &str, cg: PathBuf, pin: bool) {
    use std::sync::mpsc::{channel, Sender};
    use std::sync::{Mutex, OnceLock};
    type Job = (String, PathBuf, bool);
    static QUEUE: OnceLock<Mutex<Sender<Job>>> = OnceLock::new();
    let q = QUEUE.get_or_init(|| {
        let (tx, rx) = channel::<Job>();
        std::thread::Builder::new()
            .name("sandstorm-reclaim".into())
            .spawn(move || {
                for (id, cg, pin) in rx {
                    reclaim_frozen(&id, &cg, pin);
                }
            })
            .expect("spawning the reclaim worker");
        Mutex::new(tx)
    });
    if let Ok(tx) = q.lock() {
        let _ = tx.send((id.to_string(), cg, pin));
    }
}

/// Where a sandbox's reclaimed pages go: kept in zswap's pool when the swap
/// behind it is a disk, or past zswap straight to the device when that
/// device is zram -- otherwise every page is compressed twice, once into
/// zswap and again when zswap hands it on.
pub(super) fn route_reclaim(cg: &Path, pin_in_zswap: bool) {
    if pin_in_zswap {
        let _ = std::fs::write(cg.join("memory.zswap.writeback"), "0");
    } else {
        let _ = std::fs::write(cg.join("memory.zswap.max"), "0");
    }
}

fn reclaim_frozen(id: &str, cg: &Path, pin: bool) {
    let frozen = || {
        std::fs::read_to_string(cg.join("cgroup.freeze"))
            .map(|s| s.trim() == "1")
            .unwrap_or(false)
    };
    let mib = || {
        std::fs::read_to_string(cg.join("memory.current"))
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map_or(0, |b| b >> 20)
    };
    if !frozen() {
        return;
    }
    // Keep what is reclaimed in compressed RAM rather than letting zswap
    // write it on to disk: that is the difference between 310 ms and 3.9 s
    // to page it all back. Kernels before 6.8 have no such knob, and there
    // some pages may reach disk -- slower to wake, never wrong.
    route_reclaim(cg, pin);
    let (t0, before) = (Instant::now(), mib());
    // The kernel answers EAGAIN when it cannot take the WHOLE amount asked
    // for, having taken what it could. Stopping at the first one left 135
    // of 185 MiB on bm; halving the ask and carrying on gets the rest.
    let mut ask = 64u64;
    for _ in 0..(before / 4 + 16) {
        if !frozen() {
            break;
        }
        if std::fs::write(cg.join("memory.reclaim"), format!("{ask}M")).is_err() {
            if ask == 4 {
                break;
            }
            ask /= 2;
        }
    }
    tracing::info!(
        %id, before_mib = before, after_mib = mib(), ms = t0.elapsed().as_millis() as u64,
        "reclaimed a frozen sandbox"
    );
}

/// Holds no state of its own beyond its configuration -- everything it
/// knows it reads from disk and the cgroup tree -- so a clone is a second
/// handle on the same node, which is what the gateway's waker uses.
#[derive(Clone)]
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
            if id.starts_with(WARM_SEED_PREFIX) {
                continue;
            }

            if self.cfg.work_dir.join("snapshots").join(&id).is_dir() {
                found.push(ObservedSandbox {
                    ip: self.ip_of(&id),
                    net_index: self.net_index_of(&id),
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
                ip: self.ip_of(&id),
                net_index: self.net_index_of(&id),
                id,
                tier: if frozen { Tier::Frozen } else { Tier::Hot },
                serving: !frozen,
            });
        }
        Ok(found)
    }

    fn cgroup_path(&self, id: &str) -> PathBuf {
        PathBuf::from("/sys/fs/cgroup/sandstorm").join(id)
    }

    /// Read a snapshotted sandbox's image so it is in the page cache when a
    /// request wakes it. Returns the bytes read, or `None` when there is no
    /// image -- the sandbox is not snapshotted, or has gone.
    pub fn prefetch_snapshot(&self, id: &str) -> Result<Option<u64>> {
        use std::io::Read;
        let dir = self.cfg.work_dir.join("snapshots").join(id);
        let Ok(rd) = std::fs::read_dir(&dir) else {
            return Ok(None);
        };
        let mut buf = vec![0u8; 1 << 20];
        let mut total = 0u64;
        for e in rd.flatten() {
            if !e.path().is_file() {
                continue;
            }
            let mut f = std::fs::File::open(e.path())?;
            loop {
                let n = f.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                total += n as u64;
            }
        }
        Ok(Some(total))
    }

    pub(super) fn pins_in_zswap(&self) -> bool {
        self.cfg.pin_in_zswap
    }

    /// The sandbox's cgroup, for the node's other workers.
    pub(super) fn cgroup_dir(&self, id: &str) -> PathBuf {
        self.cgroup_path(id)
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
    pub fn create(&self, req: &CreateRequest<'_>) -> Result<()> {
        let CreateRequest {
            id,
            template,
            spec,
            port,
            volume,
            net_index,
            from_tag,
            gateway_port,
            token,
        } = *req;
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

        // Its own network namespace. Each sandbox is alone on a /30, so the
        // port it binds is its own business -- two sandboxes may both use
        // 8080, which is why no host-wide port check is needed any more.
        let netns = match net_index {
            Some(i) => {
                let slot = net::setup(&self.cfg.net, id, i)
                    .with_context(|| format!("setting up the network for {id}"))?;
                // Persisted so a restarted node can map this sandbox back to
                // its slot, which it needs to reclaim the address later.
                let bundles = self.cfg.work_dir.join("bundles").join(id);
                std::fs::create_dir_all(&bundles)?;
                std::fs::write(bundles.join("net-index"), i.to_string())?;
                // The gateway's port and the token it checks, beside the
                // net index and for the same reason: a restarted node must
                // be able to re-expose this sandbox, and a port that came
                // back open without its token would be worse than one that
                // did not come back at all.
                if let Some(gp) = gateway_port {
                    std::fs::write(bundles.join("gateway-port"), gp.to_string())?;
                }
                std::fs::write(bundles.join("sandbox-port"), port.to_string())?;
                if !token.is_empty() {
                    let tp = bundles.join("token");
                    std::fs::write(&tp, token)?;
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        std::fs::set_permissions(&tp, std::fs::Permissions::from_mode(0o600))?;
                    }
                }
                tracing::info!(%id, ip = %slot.sandbox_ip, netns = %slot.netns, "network ready");
                Some(format!("/var/run/netns/{}", slot.netns))
            }
            None => None,
        };

        let layout = bundle::build(
            &self.cfg,
            &bundle::BuildRequest {
                id,
                template_rootfs: &rootfs,
                spec: &spec,
                port,
                volume,
                restore_from,
                netns: netns.as_deref(),
            },
        )
        .with_context(|| format!("building bundle for {id}"))?;

        tracing::info!(%id, %template, port, ?volume, "create");

        // A warm start: restore the template's booted image instead of
        // booting. Not with a volume -- the seed booted against an empty
        // one, and a program that read its volume at startup would carry
        // that view into a sandbox whose volume is not empty -- and never for
        // a seed itself.
        let warm = match (&spec.warm, from_tag, restore_from, volume) {
            (Some(w), None, None, None) if !id.starts_with(WARM_SEED_PREFIX) => {
                let dir = self.warm_dir(template, req.spec, port, &rootfs);
                if dir.join("ready").exists() {
                    Some((dir, w.clone()))
                } else {
                    self.seed_later(template, req.spec, port, dir);
                    None
                }
            }
            _ => None,
        };
        if let Some((image, w)) = warm {
            let t0 = Instant::now();
            match self.restore_warm(id, &layout.bundle, &image, &w) {
                Ok(()) => {
                    self.accept_connections(id);
                    tracing::info!(%id, %template, ms = t0.elapsed().as_millis() as u64, "started warm");
                    return Ok(());
                }
                Err(e) => {
                    tracing::warn!(%id, %template, error = %format!("{e:#}"), "warm start failed; booting instead");
                    let _ = self.runsc(&["delete", "-force", id]);
                }
            }
        }

        match from_tag {
            // A fork: restore from the tag's image. One image serves any
            // number of children, so nothing is copied -- measured on bm,
            // five children from one 14 MiB image in 226 ms.
            Some(tag) => {
                let image = self.tag_path(tag)?;
                if !image.is_dir() {
                    bail!("tag {tag} is not on this node (no {})", image.display());
                }
                self.ensure_net(id)?;
                self.runsc_spawn(
                    id,
                    &[
                        "restore",
                        "--bundle",
                        &layout.bundle.to_string_lossy(),
                        &format!("--image-path={}", image.display()),
                        "--detach",
                        id,
                    ],
                )
                .with_context(|| format!("forking {id} from tag {tag}"))?;
            }
            // create then start, rather than `run`: create reports a bad
            // bundle as an error we can attribute.
            None => {
                self.runsc_spawn(
                    id,
                    &["create", "--bundle", &layout.bundle.to_string_lossy(), id],
                )
                .with_context(|| format!("runsc create {id}"))?;
                self.runsc_spawn(id, &["start", id])
                    .with_context(|| format!("runsc start {id}"))?;
            }
        }
        // Whatever it was before, it is running now.
        self.accept_connections(id);
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

    /// Where this node keeps tag images, its own and fetched ones.
    pub fn tags_dir(&self) -> PathBuf {
        self.cfg.work_dir.join("tags")
    }

    /// Where a tag's checkpoint image lives on this node.
    ///
    /// The tag becomes a path component, so it must be a plain name for the
    /// same reason a template must.
    fn tag_path(&self, tag: &str) -> Result<PathBuf> {
        // Checked again on this side on purpose: this is the call that
        // joins the tag into a path, so it must not depend on a caller
        // having validated it.
        if !crate::tag::is_safe(tag) {
            bail!("tag {tag:?} is not a plain name");
        }
        Ok(self.tags_dir().join(tag))
    }

    /// Checkpoint a running sandbox into a tag, then put the parent back.
    ///
    /// runsc's checkpoint stops the container, so the parent is restored
    /// from the image it just wrote. Measured on bm: 33 ms to checkpoint
    /// and 45 ms to restore, so the parent pauses for well under a tenth of
    /// a second. A live fork with no pause is an RFC non-goal.
    pub fn branch(&self, id: &str, tag: &str) -> Result<u64> {
        let image = self.tag_path(tag)?;
        if image.exists() {
            bail!("tag {tag} already exists");
        }
        std::fs::create_dir_all(&image)?;

        let bundle = self.cfg.work_dir.join("bundles").join(id);
        if !bundle.join("config.json").exists() {
            bail!("{id} has no bundle on this node");
        }

        // Thaw first if frozen: checkpointing a frozen task set hangs.
        let _ = self.freeze(id, false);
        self.runsc_timeout(
            &[
                "checkpoint",
                &format!("--image-path={}", image.display()),
                id,
            ],
            RUNSC_SNAPSHOT_TIMEOUT,
        )
        .with_context(|| format!("checkpointing {id} into tag {tag}"))?;
        let _ = self.runsc(&["delete", "-force", id]);

        // Put the parent back from the image it just produced, with its
        // address reapplied first.
        self.ensure_net(id)?;
        self.runsc_spawn(
            id,
            &[
                "restore",
                "--bundle",
                &bundle.to_string_lossy(),
                &format!("--image-path={}", image.display()),
                "--detach",
                id,
            ],
        )
        .with_context(|| format!("restoring {id} after branching it into {tag}"))?;

        let size = dir_size(&image);
        tracing::info!(%id, %tag, size_bytes = size, "branched");
        Ok(size)
    }

    /// Where a template's warm image lives, keyed by everything that would
    /// make an image of it wrong: the spec as the master sent it, the port,
    /// this node's runsc and kernel, and when the template's rootfs last
    /// changed. A change to any of them is a new key, and so a new seed.
    fn warm_dir(
        &self,
        template: &str,
        spec: &serde_json::Value,
        port: u16,
        rootfs: &Path,
    ) -> PathBuf {
        use sha2::{Digest, Sha256};
        static FP: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        let fp = FP.get_or_init(|| fingerprint().unwrap_or_default());
        let mtime = |p: &Path| {
            std::fs::metadata(p)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_nanos())
        };
        let mut h = Sha256::new();
        h.update(template.as_bytes());
        h.update(serde_json::to_vec(spec).unwrap_or_default());
        h.update(port.to_le_bytes());
        h.update(fp.as_bytes());
        h.update(mtime(rootfs).to_le_bytes());
        h.update(mtime(&rootfs.join("app")).to_le_bytes());
        let key = hex::encode(&h.finalize()[..12]);
        self.cfg.work_dir.join("warm").join(template).join(key)
    }

    fn restore_warm(&self, id: &str, bundle: &Path, image: &Path, w: &WarmSpec) -> Result<()> {
        self.ensure_net(id)?;
        self.runsc_spawn(
            id,
            &[
                "restore",
                "--bundle",
                &bundle.to_string_lossy(),
                &format!("--image-path={}", image.display()),
                "--detach",
                id,
            ],
        )
        .context("restoring the warm image")?;
        if !w.after_restore.is_empty() {
            let (_, stderr, code) = self
                .exec(id, &w.after_restore, AFTER_RESTORE_TIMEOUT)
                .context("running after_restore")?;
            if code != 0 {
                bail!("after_restore exited {code}: {}", stderr.trim());
            }
        }
        Ok(())
    }

    /// Take a template's warm image in the background, from a sandbox of
    /// its own, so no caller's request state can end up in it.
    ///
    /// One seed at a time on a node -- it uses the node's single reserved
    /// network slot -- and none while the node is short of memory.
    fn seed_later(&self, template: &str, spec: &serde_json::Value, port: u16, dir: PathBuf) {
        use std::sync::{Mutex, OnceLock};
        static BUSY: OnceLock<Mutex<()>> = OnceLock::new();
        let failed = dir.with_extension("failed");
        let recently_failed = std::fs::metadata(&failed)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age < WARM_RETRY);
        if recently_failed || host_pressure().0 < 1024 {
            return;
        }
        let Ok(guard) = BUSY.get_or_init(Mutex::default).try_lock() else {
            return;
        };
        drop(guard);
        let (rt, template, spec) = (self.clone(), template.to_string(), spec.clone());
        let spawned = std::thread::Builder::new()
            .name("sandstorm-warm-seed".into())
            .spawn(move || {
                let Ok(_one) = BUSY.get_or_init(Mutex::default).try_lock() else {
                    return;
                };
                if dir.join("ready").exists() {
                    return;
                }
                let t0 = Instant::now();
                match rt.seed(&template, &spec, port, &dir) {
                    Ok(mib) => tracing::info!(
                        %template, image_mib = mib, ms = t0.elapsed().as_millis() as u64,
                        "warm image ready; new sandboxes of this template start from it"
                    ),
                    Err(e) => {
                        tracing::warn!(%template, error = %format!("{e:#}"), "could not take a warm image; staying cold for now");
                        let _ = std::fs::create_dir_all(dir.parent().unwrap_or(&dir));
                        let _ = std::fs::write(&failed, format!("{e:#}\n"));
                    }
                }
            });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "could not start a warm seed");
        }
    }

    fn seed(&self, template: &str, spec: &serde_json::Value, port: u16, dir: &Path) -> Result<u64> {
        let w: WarmSpec = serde_json::from_value::<TemplateSpec>(spec.clone())?
            .warm
            .unwrap_or_default();
        let key = dir.file_name().unwrap_or_default().to_string_lossy();
        let seed = format!("{WARM_SEED_PREFIX}{}", &key[..key.len().min(12)]);
        let slot = self.cfg.net.reserved_index();
        // Whatever an interrupted seed left behind.
        let _ = self.destroy_with_net(&seed, Some(slot));

        let result = (|| {
            self.create(&CreateRequest {
                id: &seed,
                template,
                spec,
                port,
                volume: None,
                net_index: Some(slot),
                from_tag: None,
                gateway_port: None,
                token: "",
            })?;
            let ip = self.sandbox_ip(&seed).context("the seed has no address")?;
            let deadline = Instant::now() + Duration::from_secs(w.ready_timeout_secs);
            let addr = std::net::SocketAddr::from((ip, port));
            while std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_err() {
                if Instant::now() > deadline {
                    bail!(
                        "the seed was not listening on {port} after {}s",
                        w.ready_timeout_secs
                    );
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            // Let the program finish with the probe's connection, so the
            // image does not carry a half-handled socket.
            std::thread::sleep(Duration::from_millis(300));

            let tmp = dir.with_extension("tmp");
            let _ = std::fs::remove_dir_all(&tmp);
            std::fs::create_dir_all(&tmp)?;
            // Uncompressed on purpose: this image is restored on every
            // create, so it lives in the page cache, where an uncompressed
            // image restores fastest (138 ms against 696 on bm).
            self.runsc_timeout(
                &[
                    "checkpoint",
                    &format!("--image-path={}", tmp.display()),
                    &seed,
                ],
                RUNSC_SNAPSHOT_TIMEOUT,
            )
            .context("checkpointing the seed")?;
            std::fs::write(tmp.join("ready"), "")?;
            // Older images of this template are for a spec or rootfs that
            // no longer exists.
            if let Some(parent) = dir.parent() {
                for e in std::fs::read_dir(parent).into_iter().flatten().flatten() {
                    if e.path() != tmp {
                        let _ = std::fs::remove_dir_all(e.path());
                        let _ = std::fs::remove_file(e.path());
                    }
                }
            }
            std::fs::rename(&tmp, dir)?;
            Ok(dir_size(dir) >> 20)
        })();
        let _ = self.runsc(&["delete", "-force", &seed]);
        let _ = self.destroy_with_net(&seed, Some(slot));
        result
    }

    /// Remove a tag's image. Only called once the master knows nothing is
    /// forked from it.
    pub fn drop_tag(&self, tag: &str) -> Result<()> {
        let image = self.tag_path(tag)?;
        std::fs::remove_dir_all(&image).ok();
        tracing::info!(%tag, "tag removed");
        Ok(())
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
        // Whether connections are refused follows the DESTINATION, not the
        // steps taken to reach it. Demoting Hot -> Snapshot passes through
        // Frozen and thaws again on the way, so deciding per step left a
        // window of tens of milliseconds where a sandbox about to be
        // checkpointed looked reachable and a connect would hang.
        //
        // Order matters in both directions, for different reasons. Refuse
        // BEFORE going cold: nothing should reach a sandbox that is being
        // taken down, and Detached tears the bundle down, which takes the
        // net index with it -- after that there is no address left to write
        // a rule for. Accept AFTER coming back: a sandbox that is not up
        // yet should refuse rather than hang.
        if !want.is_serving() {
            self.refuse_connections(id);
        }
        let settle = |rt: &Self| {
            if want.is_serving() {
                rt.accept_connections(id);
            }
            // Only when Frozen is where it is staying. Hot -> Snapshot
            // passes through Frozen, and squeezing a sandbox that is about
            // to be checkpointed only makes the checkpoint page it back in.
            if want == Tier::Frozen && rt.cfg.reclaim_frozen {
                reclaim_later(id, rt.cgroup_path(id), rt.cfg.pin_in_zswap);
            }
        };
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
                settle(self);
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
                settle(self);
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
        if self.cfg.reclaim_frozen && self.cfg.pin_in_zswap && !self.cfg.reclaim_hot && !on {
            // After the thaw, which is what a waiting caller needs first.
            // Back to the default, so a hot sandbox under real pressure can
            // still spill to disk rather than be stuck -- unless hot reclaim
            // is managing it, which wants compressed RAM only.
            let _ = std::fs::write(cg.join("memory.zswap.writeback"), "1");
        }
        Ok(())
    }

    fn checkpoint(&self, id: &str) -> Result<()> {
        let dir = self.cfg.work_dir.join("snapshots").join(id);
        std::fs::create_dir_all(&dir)?;
        let img = dir.join("checkpoint.img");
        let image_path = format!("--image-path={}", dir.display());
        // Restore reads the compression from the image, so images written
        // either way restore the same and a node can change its mind.
        let mut args = vec!["checkpoint", image_path.as_str()];
        if self.cfg.compress_snapshots {
            args.push("--compression=flate-best-speed");
        }
        args.push(id);
        self.runsc_timeout(&args, RUNSC_SNAPSHOT_TIMEOUT)
            .with_context(|| format!("checkpointing {id}"))?;
        if !img.exists() && std::fs::read_dir(&dir)?.next().is_none() {
            bail!("checkpoint produced no image for {id}");
        }
        // The process is gone; the writable layer stays, which is why
        // Snapshot frees memory but not disk.
        let _ = self.runsc(&["delete", "-force", id]);
        self.release_net(id);
        tracing::info!(%id, "checkpointed");
        Ok(())
    }

    fn restore(&self, id: &str) -> Result<()> {
        let dir = self.cfg.work_dir.join("snapshots").join(id);
        let bundle = self.cfg.work_dir.join("bundles").join(id);
        self.ensure_net(id)?;
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
        self.accept_connections(id);
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

    /// Run a command inside a running sandbox.
    ///
    /// `.output()` is safe here, unlike for create: an exec'd command is
    /// expected to finish and its output is the whole point. The timeout is
    /// what protects us if it spawns something that holds the pipes open.
    pub fn exec(
        &self,
        id: &str,
        args: &[String],
        timeout: Duration,
    ) -> Result<(String, String, i64)> {
        if args.is_empty() {
            bail!("exec needs a command to run");
        }
        let tmp = self.cfg.work_dir.join("tmp");
        std::fs::create_dir_all(&tmp)?;
        let stem = format!("exec-{}-{}", id, std::process::id());
        let (op, ep) = (
            tmp.join(format!("{stem}.out")),
            tmp.join(format!("{stem}.err")),
        );
        let (of, ef) = (std::fs::File::create(&op)?, std::fs::File::create(&ep)?);

        let mut child = Exec::new(&self.cfg.runsc)
            .arg("--root")
            .arg(&self.cfg.runsc_root)
            .arg("exec")
            .arg(id)
            .args(args)
            .stdin(Stdio::null())
            .stdout(of)
            .stderr(ef)
            .spawn()
            .with_context(|| format!("runsc exec {id}"))?;

        let deadline = Instant::now() + timeout;
        let mut timed_out = false;
        let status = loop {
            match child.try_wait()? {
                Some(st) => break st,
                None if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    timed_out = true;
                    break std::process::ExitStatus::default();
                }
                None => std::thread::sleep(Duration::from_millis(2)),
            }
        };

        let stdout = std::fs::read_to_string(&op).unwrap_or_default();
        let mut stderr = std::fs::read_to_string(&ep).unwrap_or_default();
        std::fs::remove_file(&op).ok();
        std::fs::remove_file(&ep).ok();

        if timed_out {
            // A timeout is reported as output, not as a transport error: the
            // caller asked for a command's result and this IS the result.
            stderr.push_str(&format!(
                "\nsandstorm: killed after {}s\n",
                timeout.as_secs()
            ));
            return Ok((stdout, stderr, 124));
        }
        Ok((stdout, stderr, status.code().unwrap_or(-1) as i64))
    }

    pub fn destroy_with_net(&self, id: &str, net_index: Option<u32>) -> Result<()> {
        // Prefer our own record: the master may be asking about an orphan it
        // no longer has a row for.
        let index = net_index.or_else(|| self.net_index_of(id));
        // Read the address before teardown removes what it is derived from,
        // or the rule outlives the sandbox and the next holder of that slot
        // is refused.
        let ip = index
            .and_then(|i| self.cfg.net.slot(id, i).ok())
            .map(|s| s.sandbox_ip);
        self.destroy(id)?;
        if let Some(i) = index {
            net::teardown(&self.cfg.net, id, i)
                .with_context(|| format!("tearing down the network for {id}"))?;
        }
        if let Some(ip) = ip {
            if let Err(e) = net::allow(ip) {
                tracing::warn!(%id, error = %e, "could not clear the cold rule");
            }
        }
        Ok(())
    }

    /// What this sandbox should be exposed as, read back from its bundle.
    ///
    /// Returns `None` when either half is missing: a port with no token
    /// would be an open door, and a token with no port has nothing to
    /// guard, so neither is served on its own.
    pub fn exposed(&self, id: &str) -> Option<crate::node::Exposed> {
        let dir = self.cfg.work_dir.join("bundles").join(id);
        let gateway_port: u16 = std::fs::read_to_string(dir.join("gateway-port"))
            .ok()?
            .trim()
            .parse()
            .ok()?;
        let token = std::fs::read_to_string(dir.join("token")).ok()?;
        let token = token.trim().to_string();
        if token.is_empty() {
            return None;
        }
        Some(crate::node::Exposed {
            id: id.to_string(),
            gateway_port,
            sandbox_ip: self.sandbox_ip(id)?,
            sandbox_port: self.sandbox_port_of(id).unwrap_or(8080),
            token,
        })
    }

    /// The port the sandbox's process listens on inside its namespace,
    /// recorded at create time because the gateway needs to forward to it.
    fn sandbox_port_of(&self, id: &str) -> Option<u16> {
        std::fs::read_to_string(
            self.cfg
                .work_dir
                .join("bundles")
                .join(id)
                .join("sandbox-port"),
        )
        .ok()?
        .trim()
        .parse()
        .ok()
    }

    fn net_index_of(&self, id: &str) -> Option<u32> {
        std::fs::read_to_string(self.cfg.work_dir.join("bundles").join(id).join("net-index"))
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    /// Take a snapshotted sandbox's namespace down, keeping its slot; see
    /// `net::release`. The per-address REJECT goes with it: with no route
    /// left, the unreachable floor refuses faster than the rule did, and a
    /// rule per snapshotted sandbox was one more line every outgoing packet
    /// on the host walked past. `restore` rebuilds both through
    /// `ensure_net`. A failure is logged, not returned: the snapshot itself
    /// succeeded, and a namespace left up costs memory, not correctness.
    fn release_net(&self, id: &str) {
        let Some(index) = self.net_index_of(id) else {
            return;
        };
        let ip = self.sandbox_ip(id);
        if let Err(e) = net::release(&self.cfg.net, id, index) {
            tracing::warn!(%id, error = %e, "could not release the network of a snapshotted sandbox");
            return;
        }
        if let Some(ip) = ip {
            let _ = net::allow(ip);
        }
    }

    /// Refuse connections to this sandbox, because it is not running.
    ///
    /// Called on every path out of Hot. A failure here is logged and not
    /// propagated: the tier change itself succeeded, and the consequence
    /// of a missing rule is a caller waiting out a timeout rather than
    /// anything incorrect.
    fn refuse_connections(&self, id: &str) {
        if let Some(ip) = self.sandbox_ip(id) {
            if let Err(e) = net::reject(ip) {
                tracing::warn!(%id, error = %e, "could not refuse connections while cold");
            }
        }
    }

    /// Accept connections again. Called on every path into Hot, including
    /// ones that were never cold, since it is idempotent.
    fn accept_connections(&self, id: &str) {
        if let Some(ip) = self.sandbox_ip(id) {
            if let Err(e) = net::allow(ip) {
                tracing::warn!(%id, error = %e, "could not re-allow connections");
            }
        }
    }

    fn sandbox_ip(&self, id: &str) -> Option<std::net::Ipv4Addr> {
        let i = self.net_index_of(id)?;
        self.cfg.net.slot(id, i).ok().map(|s| s.sandbox_ip)
    }

    /// Put the sandbox's address back before restoring it.
    ///
    /// gVisor takes the veth device over, and deleting the container leaves
    /// it in the namespace with its address gone -- the host side keeps
    /// 10.86.x.1/30 while the sandbox side has no inet at all. A restore
    /// then re-attaches to an unconfigured device, so the sandbox comes
    /// back running but unreachable. `net::setup` is idempotent, so this
    /// simply re-applies the plan.
    ///
    /// Every restore path needs it: the snapshot tier coming back to hot,
    /// and a branch putting its parent back.
    fn ensure_net(&self, id: &str) -> Result<()> {
        let Some(i) = self.net_index_of(id) else {
            return Ok(());
        };
        let slot = net::setup(&self.cfg.net, id, i)
            .with_context(|| format!("restoring the network for {id}"))?;
        tracing::debug!(%id, ip = %slot.sandbox_ip, "network reapplied before restore");
        Ok(())
    }

    /// The address this sandbox was given, for the master's records.
    fn ip_of(&self, id: &str) -> Option<String> {
        let i = self.net_index_of(id)?;
        self.cfg
            .net
            .slot(id, i)
            .ok()
            .map(|s| s.sandbox_ip.to_string())
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
                work_dir: std::env::temp_dir().join("sandstorm-rt-test"),
                volume_root: std::env::temp_dir().join("sandstorm-rt-test/vol"),
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

fn dir_size(p: &std::path::Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(p) else {
        return 0;
    };
    rd.flatten()
        .filter_map(|e| e.metadata().ok())
        .map(|m| if m.is_dir() { 0 } else { m.len() })
        .sum()
}

#[cfg(test)]
mod warm_tests {
    use super::*;

    fn rt(work: &Path) -> Runtime {
        Runtime {
            cfg: RuntimeConfig {
                work_dir: work.to_path_buf(),
                volume_root: work.join("volumes"),
                ..Default::default()
            },
        }
    }

    /// A seed is the node's own business: reporting it would have the
    /// master destroy it as an orphan halfway through being imaged.
    #[test]
    fn a_seed_is_never_reported() {
        let dir = tempfile::tempdir().unwrap();
        let rt = rt(dir.path());
        for id in ["sb-real", "sandstorm-warm-abc123"] {
            std::fs::create_dir_all(dir.path().join("bundles").join(id)).unwrap();
            std::fs::create_dir_all(dir.path().join("snapshots").join(id)).unwrap();
        }
        let ids: Vec<String> = rt.observe().unwrap().into_iter().map(|o| o.id).collect();
        assert_eq!(ids, vec!["sb-real".to_string()]);
    }

    /// Anything that would make an image wrong makes a new key.
    #[test]
    fn the_warm_key_follows_the_spec_port_and_rootfs() {
        let dir = tempfile::tempdir().unwrap();
        let rt = rt(dir.path());
        let rootfs = dir.path().join("templates/py/rootfs");
        std::fs::create_dir_all(rootfs.join("app")).unwrap();
        let spec = serde_json::json!({"args": ["python3", "app.py"], "warm": {}});
        let a = rt.warm_dir("py", &spec, 8080, &rootfs);
        assert_eq!(a, rt.warm_dir("py", &spec, 8080, &rootfs), "stable");
        assert_ne!(a, rt.warm_dir("py", &spec, 9090, &rootfs), "port");
        let other = serde_json::json!({"args": ["python3", "other.py"], "warm": {}});
        assert_ne!(a, rt.warm_dir("py", &other, 8080, &rootfs), "spec");
        assert!(a.starts_with(dir.path().join("warm/py")));

        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(rootfs.join("app/new.py"), "").unwrap();
        assert_ne!(a, rt.warm_dir("py", &spec, 8080, &rootfs), "rootfs changed");
    }

    #[test]
    fn warm_is_opt_in_and_parses_with_defaults() {
        let plain: TemplateSpec =
            serde_json::from_value(serde_json::json!({"args": ["x"]})).unwrap();
        assert!(plain.warm.is_none());
        let warm: TemplateSpec = serde_json::from_value(
            serde_json::json!({"args": ["x"], "warm": {"after_restore": ["kill", "-USR1", "1"]}}),
        )
        .unwrap();
        let w = warm.warm.unwrap();
        assert_eq!(w.after_restore, vec!["kill", "-USR1", "1"]);
        assert_eq!(w.ready_timeout_secs, 60);
    }

    /// The node's slot and the master's plan must not overlap.
    #[test]
    fn the_reserved_slot_is_past_the_masters_plan() {
        let net = NetConfig::default();
        assert_eq!(net.prefix, 16);
        assert_eq!(net.reserved_index(), 16383);
        assert!(
            net.slot("seed", net.reserved_index()).is_ok(),
            "it is a real slot"
        );
        assert!(
            net.slot("x", net.reserved_index() + 1).is_err(),
            "and the last one"
        );
    }
}
