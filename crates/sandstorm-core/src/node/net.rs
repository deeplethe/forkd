//! A network namespace per sandbox.
//!
//! Sandboxes used to share the host's namespace (`--network=host`). Measured
//! on bm, that let code inside a sandbox connect to the master's API, to the
//! host's sshd, and to sibling sandboxes. Each sandbox now gets its own
//! namespace and its own address, reachable from the node and from nowhere
//! else.
//!
//! The arrangement, verified by hand before any of this was written:
//!
//! ```text
//!   node                         sandbox n
//!   sst<n>h  10.86.x.y+1/30  <-> sst<n>s  10.86.x.y+2/30   (its netns)
//! ```
//!
//! A /30 per sandbox rather than a shared bridge: with no layer-2 segment in
//! common there is nothing for sandboxes to talk over, so isolation does not
//! depend on getting bridge port isolation right.
//!
//! Three firewall rules do the rest, and they are explicit rather than
//! relying on `ip_forward=0`: that sysctl belongs to whoever installed
//! Docker, and this host's FORWARD policy is ACCEPT. Verified with
//! `ip_forward=1`: siblings, the host's sshd and the internet all stayed
//! unreachable while the node could still reach the sandbox.

use anyhow::{bail, Context, Result};
use std::net::Ipv4Addr;
use std::process::Command as Exec;

/// Address plan for one node.
#[derive(Debug, Clone)]
pub struct NetConfig {
    /// First address of the range carved into /30s. 10.86.0.0 by default,
    /// which gives 16384 sandboxes out of a /16.
    pub base: Ipv4Addr,
    /// Prefix of the whole range, used for the firewall rules.
    pub prefix: u8,
}

impl Default for NetConfig {
    fn default() -> Self {
        Self {
            base: Ipv4Addr::new(10, 86, 0, 0),
            prefix: 16,
        }
    }
}

/// One sandbox's slot in the plan.
/// A short, stable tag for a sandbox's veth names.
///
/// Named from the sandbox id, not its network index, because **both ends of
/// a veth pair are created in the host's namespace** before one is moved
/// into the sandbox's -- so both names have to be unique across the whole
/// machine. The index is allocated per node, so two nodes on one machine
/// both start at 0 and collided: `Cannot find device "sst0s"`, which backoff
/// and retry papered over. A hash rather than a slice of the id because a
/// caller may supply an id that is not hex.
///
/// 10 hex characters keeps `sst<tag>h` at 14 bytes, inside IFNAMSIZ's 15.
fn if_tag(id: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(id.as_bytes()))[..10].to_string()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slot {
    pub index: u32,
    /// The node's end of the veth.
    pub host_ip: Ipv4Addr,
    /// The sandbox's address, which is how anything reaches it.
    pub sandbox_ip: Ipv4Addr,
    /// Named after the sandbox, not its slot: after a restart the node has
    /// an id and must find the namespace from it.
    pub netns: String,
    host_if: String,
    sandbox_if: String,
}

impl NetConfig {
    pub fn subnet(&self) -> String {
        format!("{}/{}", self.base, self.prefix)
    }

    /// The one slot the master never hands out, kept for the node's own
    /// use: the seed sandbox a warm template image is taken from. The
    /// master's plan stops one short of it (`max_net_index` in main.rs).
    pub fn reserved_index(&self) -> u32 {
        (1u32 << (32 - self.prefix.min(30) as u32)) / 4 - 1
    }

    /// The /30 for one index. Interface names stay inside the kernel's
    /// 15-character limit even at the top of the range.
    pub fn slot(&self, id: &str, index: u32) -> Result<Slot> {
        let span = 1u32 << (32 - self.prefix as u32);
        let offset = index
            .checked_mul(4)
            .filter(|o| o + 4 <= span)
            .with_context(|| {
                format!(
                    "index {index} does not fit in {} -- the node is out of addresses",
                    self.subnet()
                )
            })?;
        let base: u32 = self.base.into();
        Ok(Slot {
            index,
            host_ip: Ipv4Addr::from(base + offset + 1),
            sandbox_ip: Ipv4Addr::from(base + offset + 2),
            netns: format!("sandstorm-{id}"),
            host_if: format!("sst{}h", if_tag(id)),
            sandbox_if: format!("sst{}s", if_tag(id)),
        })
    }
}

fn run(what: &str, args: &[&str]) -> Result<()> {
    let out = Exec::new(args[0])
        .args(&args[1..])
        .output()
        .with_context(|| format!("running {:?}", args))?;
    if !out.status.success() {
        bail!(
            "{what}: {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn ok(args: &[&str]) -> bool {
    Exec::new(args[0])
        .args(&args[1..])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Build the namespace and veth for one sandbox. Idempotent: an existing
/// namespace is reused, which is what makes re-adoption after a restart free.
/// The interfaces in `ip -o -4 addr show` output that are a sandbox's host
/// end (`sst...h`) and not `ours`: veths left behind holding an address.
fn stale_holders(ip_output: &str, ours: &str) -> Vec<String> {
    ip_output
        .lines()
        .filter_map(|l| l.split_whitespace().nth(1))
        .map(|name| name.split('@').next().unwrap_or(name))
        .filter(|name| name.starts_with("sst") && name.ends_with('h') && *name != ours)
        .map(str::to_string)
        .collect()
}

pub fn setup(cfg: &NetConfig, id: &str, index: u32, pool: Option<&Pool>) -> Result<Slot> {
    let s = cfg.slot(id, index)?;

    if !std::path::Path::new(&format!("{NETNS_DIR}/{}", s.netns)).exists() {
        // A namespace built ahead of time, if there is one: what is left is
        // naming and addressing it. Anything that goes wrong on the way
        // falls through to building it here, as without a pool.
        if let Some(blank) = pool.and_then(Pool::take) {
            let t0 = std::time::Instant::now();
            match claim(&blank, &s) {
                Ok(()) => {
                    tracing::debug!(%id, ms = t0.elapsed().as_millis() as u64, "claimed a pooled namespace");
                    return Ok(s);
                }
                Err(e) => {
                    tracing::warn!(%id, error = %e, "could not use a pooled namespace; building one");
                    let _ = run("removing netns", &["ip", "netns", "del", &s.netns]);
                    let _ = run("removing netns", &["ip", "netns", "del", &blank.netns]);
                    let _ = run("removing veth", &["ip", "link", "del", &blank.host_if]);
                    let _ = run("removing veth", &["ip", "link", "del", &s.host_if]);
                }
            }
        }
        run("creating netns", &["ip", "netns", "add", &s.netns])?;
    }
    if !ok(&["ip", "link", "show", &s.host_if]) {
        run(
            "creating veth",
            &[
                "ip",
                "link",
                "add",
                &s.host_if,
                "type",
                "veth",
                "peer",
                "name",
                &s.sandbox_if,
            ],
        )?;
        run(
            "moving the peer into the netns",
            &["ip", "link", "set", &s.sandbox_if, "netns", &s.netns],
        )?;
    }

    let host_cidr = format!("{}/30", s.host_ip);
    let sb_cidr = format!("{}/30", s.sandbox_ip);
    let gw = s.host_ip.to_string();
    // An address is one slot's, but a veth left behind by a sandbox that
    // died outlives the slot being handed to someone else. Found on f9: two
    // host interfaces with 10.86.0.253/30, the route to .254 through the
    // dead one, and the new sandbox, running, unreachable for good.
    let held = Exec::new("ip")
        .args([
            "-o",
            "-4",
            "addr",
            "show",
            "to",
            &format!("{}/32", s.host_ip),
        ])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    for stale in stale_holders(&held, &s.host_if) {
        tracing::warn!(%id, interface = %stale, ip = %s.host_ip, "removing a stale interface holding this slot's address");
        let _ = run("removing a stale veth", &["ip", "link", "del", &stale]);
    }
    // Re-running these on an existing slot is an error we ignore: the point
    // is the end state, not whether this call is the one that got there.
    let _ = run(
        "addr",
        &["ip", "addr", "add", &host_cidr, "dev", &s.host_if],
    );
    run("host link up", &["ip", "link", "set", &s.host_if, "up"])?;
    let _ = run(
        "sandbox addr",
        &[
            "ip",
            "-n",
            &s.netns,
            "addr",
            "add",
            &sb_cidr,
            "dev",
            &s.sandbox_if,
        ],
    );
    run(
        "sandbox link up",
        &["ip", "-n", &s.netns, "link", "set", &s.sandbox_if, "up"],
    )?;
    run(
        "loopback up",
        &["ip", "-n", &s.netns, "link", "set", "lo", "up"],
    )?;
    let _ = run(
        "default route",
        &["ip", "-n", &s.netns, "route", "add", "default", "via", &gw],
    );
    Ok(s)
}

const NETNS_DIR: &str = "/var/run/netns";

/// The backend's own setup inside a pooled namespace (`Backend::prepare_netns`).
pub type Prepare = Box<dyn Fn(&str) -> Result<()> + Send + Sync>;

/// How long the pool waits after a namespace is taken before building the
/// next. Building one takes the kernel's RTNL lock, and so does every step of
/// a claim: refilling straight away put a claim behind it, 27-37 ms on bm
/// against a few with the lock to itself. The sandbox that took one is
/// usually still being restored, and a burst takes several in a row.
const REFILL_QUIET: std::time::Duration = std::time::Duration::from_millis(250);

/// Namespaces built before any sandbox asks for one.
///
/// Building a namespace is about a dozen `ip` processes, and for a
/// Firecracker sandbox another nine for its tap and NAT rules, all on the
/// path of a create or a wake from Snapshot. Measured on bm with warm
/// templates: CubeSandbox, which keeps tap devices ready, answered in 55 ms
/// against 161 ms here, and only 70-76 ms of ours was the restore itself.
///
/// None of that work depends on the sandbox. Its address comes from the
/// master's index and its names from its id, so a pooled namespace is built
/// with neither -- the namespace, the veth with its peer inside, loopback
/// up, and whatever the backend puts in it -- and `claim` renames and
/// addresses it when a sandbox takes it. An empty pool is no error: `setup`
/// builds as it always did.
pub struct Pool {
    target: usize,
    /// Keeps two nodes on one machine out of each other's pool, the same
    /// problem `if_tag` solves for sandboxes.
    tag: String,
    prepare: Prepare,
    state: std::sync::Mutex<PoolState>,
    wanted: std::sync::Condvar,
}

#[derive(Default)]
struct PoolState {
    ready: Vec<Blank>,
    next: u64,
    last_take: Option<std::time::Instant>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blank {
    netns: String,
    host_if: String,
    sandbox_if: String,
}

impl Pool {
    /// Start keeping `target` namespaces ready. `key` tells this node's
    /// pool from another's on the same machine (its work directory);
    /// `prepare` is the backend's own setup inside a namespace.
    ///
    /// Whatever a previous run of this node left in its pool is removed
    /// first: a pooled namespace holds nothing of a sandbox's.
    pub fn start(target: usize, key: &str, prepare: Prepare) -> std::sync::Arc<Self> {
        // Numbering starts somewhere new each run. Deleting a namespace
        // returns before the kernel has destroyed the veth in it, so the
        // names a previous run used can still be taken a moment later:
        // seen on bm, the first build after a restart failed with "File
        // exists" on the old run's first name.
        let next = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros() as u64)
            .unwrap_or_default();
        let pool = std::sync::Arc::new(Self {
            target,
            tag: if_tag(key)[..6].to_string(),
            prepare,
            state: std::sync::Mutex::new(PoolState {
                ready: Vec::new(),
                next,
                last_take: None,
            }),
            wanted: Default::default(),
        });
        let prefix = pool.netns_prefix();
        if let Ok(rd) = std::fs::read_dir(NETNS_DIR) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with(&prefix) {
                    // The peer goes with the namespace, and the host end
                    // with its peer.
                    let _ = run("removing a pooled netns", &["ip", "netns", "del", &name]);
                }
            }
        }
        let p = pool.clone();
        std::thread::Builder::new()
            .name("netns-pool".into())
            .spawn(move || p.fill())
            .expect("spawning the netns pool");
        pool
    }

    fn netns_prefix(&self) -> String {
        format!("sstpool-{}-", self.tag)
    }

    /// Names for the `n`th namespace this pool builds. The interface names
    /// have to be unique on the machine, like a sandbox's, and fit
    /// IFNAMSIZ: `sp` + 6 + 6 + 1 is 15.
    fn blank(&self, n: u64) -> Blank {
        let n = n & 0xff_ffff;
        Blank {
            netns: format!("{}{n}", self.netns_prefix()),
            host_if: format!("sp{}{n:06x}h", self.tag),
            sandbox_if: format!("sp{}{n:06x}s", self.tag),
        }
    }

    /// One namespace, if one is ready, and a nudge to build its successor.
    pub fn take(&self) -> Option<Blank> {
        let b = {
            let mut st = self.state.lock().unwrap();
            st.last_take = Some(std::time::Instant::now());
            st.ready.pop()
        };
        self.wanted.notify_one();
        b
    }

    /// How much longer to stay out of the way of a claim; see `REFILL_QUIET`.
    fn quiet_left(&self) -> Option<std::time::Duration> {
        let since = self.state.lock().unwrap().last_take?.elapsed();
        REFILL_QUIET.checked_sub(since).filter(|d| !d.is_zero())
    }

    fn fill(&self) {
        loop {
            let n = {
                let mut st = self.state.lock().unwrap();
                while st.ready.len() >= self.target {
                    st = self.wanted.wait(st).unwrap();
                }
                st.next += 1;
                st.next
            };
            while let Some(wait) = self.quiet_left() {
                std::thread::sleep(wait);
            }
            let b = self.blank(n);
            match self.build(&b) {
                Ok(()) => self.state.lock().unwrap().ready.push(b),
                Err(e) => {
                    tracing::warn!(netns = %b.netns, error = %e, "could not build a pooled namespace");
                    let _ = run("removing netns", &["ip", "netns", "del", &b.netns]);
                    std::thread::sleep(std::time::Duration::from_secs(5));
                }
            }
        }
    }

    fn build(&self, b: &Blank) -> Result<()> {
        run("creating netns", &["ip", "netns", "add", &b.netns])?;
        // Both ends are created in the host's namespace and the peer moved
        // in; `ip` does that in one call.
        run(
            "creating veth",
            &[
                "ip",
                "link",
                "add",
                &b.host_if,
                "type",
                "veth",
                "peer",
                "name",
                &b.sandbox_if,
                "netns",
                &b.netns,
            ],
        )?;
        run(
            "loopback up",
            &["ip", "-n", &b.netns, "link", "set", "lo", "up"],
        )?;
        (self.prepare)(&b.netns)
    }
}

/// What `claim` asks of `ip -batch`, as two scripts: the host's side and
/// the namespace's. An interface is renamed while it is still down.
fn claim_scripts(b: &Blank, s: &Slot) -> (String, String) {
    let host = format!(
        "link set dev {} name {}\naddr add {}/30 dev {}\nlink set dev {} up\n",
        b.host_if, s.host_if, s.host_ip, s.host_if, s.host_if
    );
    let inside = format!(
        "link set dev {} name {}\naddr add {}/30 dev {}\nlink set dev {} up\nroute add default via {}\n",
        b.sandbox_if, s.sandbox_if, s.sandbox_ip, s.sandbox_if, s.sandbox_if, s.host_ip
    );
    (host, inside)
}

/// Turn a pooled namespace into this sandbox's: the same names and
/// addresses `setup` would have built, so nothing after it can tell.
fn claim(b: &Blank, s: &Slot) -> Result<()> {
    rename_netns(&b.netns, &s.netns)?;
    // A veth left behind by a dead sandbox may hold this slot's address;
    // see `setup`.
    let held = Exec::new("ip")
        .args([
            "-o",
            "-4",
            "addr",
            "show",
            "to",
            &format!("{}/32", s.host_ip),
        ])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    for stale in stale_holders(&held, &s.host_if) {
        tracing::warn!(interface = %stale, ip = %s.host_ip, "removing a stale interface holding this slot's address");
        let _ = run("removing a stale veth", &["ip", "link", "del", &stale]);
    }
    let (host, inside) = claim_scripts(b, s);
    batch(&["ip", "-batch", "-"], &host).context("naming and addressing the host's end")?;
    batch(&["ip", "-n", &s.netns, "-batch", "-"], &inside)
        .context("naming and addressing the sandbox's end")?;
    Ok(())
}

fn batch(args: &[&str], script: &str) -> Result<()> {
    use std::io::Write;
    let mut child = Exec::new(args[0])
        .args(&args[1..])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("running {args:?}"))?;
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(script.as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!(
            "{args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Give a named namespace another name. A name is a bind mount of the
/// namespace under /var/run/netns, which `rename` cannot move, so this is
/// what `ip netns attach` does: bind it under the new name, then drop the
/// old one.
#[cfg(target_os = "linux")]
fn rename_netns(from: &str, to: &str) -> Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let dir = std::path::Path::new(NETNS_DIR);
    let (src, dst) = (dir.join(from), dir.join(to));
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&dst)
        .with_context(|| format!("creating {}", dst.display()))?;
    let c = |p: &std::path::Path| CString::new(p.as_os_str().as_bytes()).expect("no NUL in a path");
    let (s, d) = (c(&src), c(&dst));
    // SAFETY: both are valid NUL-terminated paths; a bind mount takes no
    // filesystem type or data.
    let r = unsafe {
        libc::mount(
            s.as_ptr(),
            d.as_ptr(),
            std::ptr::null(),
            libc::MS_BIND,
            std::ptr::null(),
        )
    };
    if r != 0 {
        let e = std::io::Error::last_os_error();
        let _ = std::fs::remove_file(&dst);
        bail!("binding {from} as {to}: {e}");
    }
    // SAFETY: a valid path; MNT_DETACH cannot fail on a busy mount.
    unsafe { libc::umount2(s.as_ptr(), libc::MNT_DETACH) };
    let _ = std::fs::remove_file(&src);
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn rename_netns(_from: &str, _to: &str) -> Result<()> {
    bail!("network namespaces are Linux only")
}

/// Take down a sandbox's namespace and veth while keeping its slot.
///
/// For the Snapshot tier: the process is gone and the image is on disk, so
/// nothing needs the kernel objects until a restore, which rebuilds them
/// with `setup` -- same index, same addresses, same names. They are not
/// small. Measured on the load-test fleet at ~0.4 MiB of unreclaimable
/// slab per namespace: 1538 snapshotted sandboxes held 694 MiB on a 1.9 GiB
/// node, which is what capped the cluster at 7284 sandboxes, not memory
/// for the sandboxes and not disk. The slot itself is a number in the
/// master's plan and costs nothing to keep.
pub fn release(cfg: &NetConfig, id: &str, index: u32) -> Result<()> {
    teardown(cfg, id, index)
}

pub fn teardown(cfg: &NetConfig, id: &str, index: u32) -> Result<()> {
    let s = cfg.slot(id, index)?;
    // Deleting the netns takes the peer with it; the host end has to go too.
    let _ = run("removing netns", &["ip", "netns", "del", &s.netns]);
    let _ = run("removing veth", &["ip", "link", "del", &s.host_if]);
    Ok(())
}

/// Chain holding one REJECT per sandbox that is not currently hot.
///
/// A dedicated chain rather than rules in OUTPUT directly: a node with a
/// hundred cold sandboxes would otherwise put a hundred rules at the top
/// of OUTPUT, in front of everything else the host does.
const COLD_CHAIN: &str = "SANDSTORM-COLD";

/// Refuse connections to a sandbox that is not running.
///
/// A frozen sandbox keeps its namespace, veth up and nothing answering, so
/// a SYN is dropped rather than rejected and the caller waits out its own
/// timeout. (A snapshotted one no longer has a namespace at all -- see
/// `release` -- and the unreachable floor in `install_rules` refuses for
/// it instead, so this rule is taken off again once it gets there.) Measured before this existed:
/// `curl -m 5` against a snapshotted sandbox took 5011 ms, and `-m 1`
/// took 1019 ms. The duration was purely the caller's deadline.
///
/// `tcp-reset` rather than the default ICMP: a RST is what a client reads
/// as "connection refused" immediately, which is the answer a caller
/// wants.
pub fn reject(ip: Ipv4Addr) -> Result<()> {
    let d = ip.to_string();
    let rule = cold_rule(&d);
    let mut check = vec!["iptables", "-C", COLD_CHAIN];
    check.extend_from_slice(&rule);
    if ok(&check) {
        return Ok(());
    }
    let mut add = vec!["iptables", "-A", COLD_CHAIN];
    add.extend_from_slice(&rule);
    run("rejecting a cold sandbox", &add)?;
    tracing::debug!(%ip, "connections refused while not hot");
    Ok(())
}

/// Let connections through again, once the sandbox is running.
///
/// Idempotent and safe to call for a sandbox that was never rejected,
/// which is what makes it correct to call on every path into Hot rather
/// than only the ones that came from a cold tier.
pub fn allow(ip: Ipv4Addr) -> Result<()> {
    let d = ip.to_string();
    let rule = cold_rule(&d);
    // Delete until it is gone: a repeated add would otherwise leave
    // duplicates that each need removing.
    loop {
        let mut check = vec!["iptables", "-C", COLD_CHAIN];
        check.extend_from_slice(&rule);
        if !ok(&check) {
            break;
        }
        let mut del = vec!["iptables", "-D", COLD_CHAIN];
        del.extend_from_slice(&rule);
        run("allowing a sandbox again", &del)?;
    }
    Ok(())
}

fn cold_rule(dest: &str) -> Vec<&str> {
    vec![
        "-d",
        dest,
        "-p",
        "tcp",
        "-j",
        "REJECT",
        "--reject-with",
        "tcp-reset",
    ]
}

/// The rules that make the namespace worth having. Called once at node start.
///
/// `INPUT` accepts established traffic from the range so the node's own
/// connections get their replies, and drops everything else from it, which
/// is what shuts out the host's own services -- sshd listens on 0.0.0.0, so
/// it answers on the veth too. `FORWARD` drops both directions, which covers
/// siblings and the internet without depending on `ip_forward`.
pub fn install_rules(cfg: &NetConfig) -> Result<()> {
    let subnet = cfg.subnet();
    let rules: &[&[&str]] = &[
        &[
            "-s",
            &subnet,
            "-m",
            "conntrack",
            "--ctstate",
            "ESTABLISHED,RELATED",
            "-j",
            "ACCEPT",
        ],
        &["-s", &subnet, "-j", "DROP"],
    ];
    // Inserted at the top: this host already has a firewall chain on INPUT,
    // and an ACCEPT there would otherwise win.
    for (pos, rule) in rules.iter().enumerate() {
        let mut check = vec!["iptables", "-C", "INPUT"];
        check.extend_from_slice(rule);
        if ok(&check) {
            continue;
        }
        let at = (pos + 1).to_string();
        let mut ins = vec!["iptables", "-I", "INPUT", &at];
        ins.extend_from_slice(rule);
        run("installing an INPUT rule", &ins)?;
    }
    for (pos, rule) in [
        vec!["-s", &subnet, "-j", "DROP"],
        vec!["-d", &subnet, "-j", "DROP"],
    ]
    .iter()
    .enumerate()
    {
        let mut check = vec!["iptables", "-C", "FORWARD"];
        check.extend_from_slice(rule);
        if ok(&check) {
            continue;
        }
        let at = (pos + 1).to_string();
        let mut ins = vec!["iptables", "-I", "FORWARD", &at];
        ins.extend_from_slice(rule);
        run("installing a FORWARD rule", &ins)?;
    }
    // The chain that holds one REJECT per cold sandbox, jumped to from
    // OUTPUT so a connect to a sandbox that is not running is refused
    // rather than left to time out.
    if !ok(&["iptables", "-n", "-L", COLD_CHAIN]) {
        run("creating the cold chain", &["iptables", "-N", COLD_CHAIN])?;
    }
    if !ok(&["iptables", "-C", "OUTPUT", "-j", COLD_CHAIN]) {
        run(
            "jumping to the cold chain",
            &["iptables", "-I", "OUTPUT", "1", "-j", COLD_CHAIN],
        )?;
    }
    // A node that restarted may have left rules for sandboxes it has
    // since forgotten; the tier each one is in is re-derived on the first
    // poll, so start from none rather than from a stale set.
    let _ = run("clearing the cold chain", &["iptables", "-F", COLD_CHAIN]);

    // A floor under the whole range. A sandbox with its namespace up has a
    // /30 route through its veth, which is more specific and wins. One in
    // Snapshot has had its namespace taken down (`release`), and without
    // this its address would follow the default route off the machine --
    // UDP and ICMP and all, possibly to a real host that has that address
    // in the VPC. With it, anything sent there fails at once with "no
    // route to host", which the gateway reads as cold and wakes on, and
    // no per-sandbox rule is needed for the whole Snapshot tier.
    run(
        "installing the unreachable floor",
        &["ip", "route", "replace", "unreachable", &subnet],
    )?;

    tracing::info!(subnet = %subnet, "sandbox network rules in place");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn veth_names_are_per_sandbox_not_per_index() {
        let cfg = NetConfig {
            base: "10.86.0.0".parse().unwrap(),
            prefix: 16,
        };
        // Two nodes allocate indices independently, so both hand out 0.
        // Before this was keyed on the id, both sandboxes wanted `sst0h`
        // and `sst0s` -- and a veth's two ends are created in the HOST
        // namespace, so the second one failed with `Cannot find device`.
        let a = cfg.slot("sb-aaaa", 0).unwrap();
        let b = cfg.slot("sb-bbbb", 0).unwrap();
        assert_ne!(a.host_if, b.host_if, "same index, different sandbox");
        assert_ne!(a.sandbox_if, b.sandbox_if);
    }

    #[test]
    fn a_stale_veth_holding_the_address_is_found_and_ours_is_not() {
        let out = "61887: ssteb59b97a8ah    inet 10.86.0.253/30 scope global ssteb59b97a8ah\\       valid_lft forever preferred_lft forever\n\
                   62387: sst7e223fa802h    inet 10.86.0.253/30 scope global sst7e223fa802h\\       valid_lft forever preferred_lft forever\n\
                   2: eth0    inet 10.86.0.253/30 scope global eth0\n";
        assert_eq!(stale_holders(out, "sst7e223fa802h"), ["ssteb59b97a8ah"]);
        assert!(stale_holders(out, "ssteb59b97a8ah").contains(&"sst7e223fa802h".to_string()));
        assert!(stale_holders("", "sstx").is_empty());
    }

    #[test]
    fn veth_names_fit_ifnamsiz() {
        let cfg = NetConfig {
            base: "10.86.0.0".parse().unwrap(),
            prefix: 16,
        };
        // IFNAMSIZ is 16 including the NUL, so 15 usable. A name over that
        // is refused by the kernel at creation time.
        for id in ["sb-0", &format!("sb-{}", "f".repeat(64))] {
            let s = cfg.slot(id, 3).unwrap();
            assert!(s.host_if.len() <= 15, "{} is too long", s.host_if);
            assert!(s.sandbox_if.len() <= 15);
        }
    }

    #[test]
    fn the_same_sandbox_always_gets_the_same_names() {
        let cfg = NetConfig {
            base: "10.86.0.0".parse().unwrap(),
            prefix: 16,
        };
        // A restart has to find the interfaces it created last time.
        assert_eq!(
            cfg.slot("sb-stable", 7).unwrap().host_if,
            cfg.slot("sb-stable", 9).unwrap().host_if,
            "the name must not move when the index does"
        );
    }

    #[test]
    fn slots_are_four_apart_and_do_not_overlap() {
        let c = NetConfig::default();
        let a = c.slot("sb-a", 0).unwrap();
        let b = c.slot("sb-b", 1).unwrap();
        assert_eq!(a.host_ip, Ipv4Addr::new(10, 86, 0, 1));
        assert_eq!(a.sandbox_ip, Ipv4Addr::new(10, 86, 0, 2));
        assert_eq!(b.host_ip, Ipv4Addr::new(10, 86, 0, 5));
        assert_eq!(b.sandbox_ip, Ipv4Addr::new(10, 86, 0, 6));
    }

    /// Each sandbox is alone on its /30, so there is no shared segment for
    /// siblings to talk over in the first place.
    #[test]
    fn no_two_slots_share_an_address() {
        let c = NetConfig::default();
        let mut seen = std::collections::HashSet::new();
        for i in 0..2000 {
            let s = c.slot("sb", i).unwrap();
            assert!(seen.insert(s.host_ip), "host ip reused at {i}");
            assert!(seen.insert(s.sandbox_ip), "sandbox ip reused at {i}");
        }
    }

    /// The kernel truncates an interface name over 15 characters, which
    /// would silently collide two sandboxes onto one device.
    #[test]
    fn interface_names_fit_the_kernel_limit() {
        let c = NetConfig::default();
        for i in [0u32, 1, 999, 16383] {
            let s = c.slot("sb-0123456789ab", i).unwrap();
            assert!(s.host_if.len() <= 15, "{} too long", s.host_if);
            assert!(s.sandbox_if.len() <= 15, "{} too long", s.sandbox_if);
            assert_ne!(s.host_if, s.sandbox_if);
        }
    }

    #[test]
    fn running_out_of_addresses_is_an_error_not_a_wrap() {
        let c = NetConfig::default();
        // A /16 holds 16384 /30s, so 16383 is the last one.
        assert!(c.slot("sb", 16383).is_ok());
        assert!(c.slot("sb", 16384).is_err());
        let e = c.slot("sb", 99_999).unwrap_err().to_string();
        assert!(e.contains("out of addresses"), "got {e}");
    }

    /// The namespace is named after the sandbox so a restarted node can
    /// find it from the id alone; the interfaces are named after the slot
    /// so they fit the kernel's 15-character limit.
    /// A RST, not an ICMP unreachable: a client reads a reset as
    /// "connection refused" at once, which is the whole point.
    #[test]
    fn the_cold_rule_resets_rather_than_dropping() {
        let r = cold_rule("10.86.0.2");
        assert!(r.contains(&"REJECT"));
        assert!(
            r.contains(&"tcp-reset"),
            "a drop would make the caller wait"
        );
        assert_eq!(r[0], "-d");
        assert_eq!(r[1], "10.86.0.2");
    }

    #[test]
    fn the_namespace_is_named_for_the_sandbox() {
        let c = NetConfig::default();
        let s = c.slot("sb-deadbeef0123", 42).unwrap();
        assert_eq!(s.netns, "sandstorm-sb-deadbeef0123");
        // The interface names used to be `sst42h`/`sst42s`, from the index.
        // They are keyed on the sandbox now -- see `if_tag` -- because the
        // index is per node and two nodes on one machine collided.
        assert!(s.host_if.starts_with("sst") && s.host_if.ends_with('h'));
        assert!(
            !s.host_if.contains("42"),
            "{} still carries the index",
            s.host_if
        );
    }

    fn pool(key: &str) -> Pool {
        Pool {
            target: 0,
            tag: if_tag(key)[..6].to_string(),
            prepare: Box::new(|_| Ok(())),
            state: Default::default(),
            wanted: Default::default(),
        }
    }

    #[test]
    fn pooled_names_fit_ifnamsiz_and_stay_apart() {
        let p = pool("/var/lib/sandstorm");
        for n in [1u64, 0xff_ffff, 1 << 40] {
            let b = p.blank(n);
            assert!(b.host_if.len() <= 15, "{} too long", b.host_if);
            assert!(b.sandbox_if.len() <= 15, "{} too long", b.sandbox_if);
            assert!(b.netns.starts_with(&p.netns_prefix()));
        }
        assert_ne!(p.blank(1), p.blank(2));
        // Two nodes on one machine: neither may take, or clear at start,
        // the other's.
        let q = pool("/var/lib/sandstorm-2");
        assert_ne!(p.netns_prefix(), q.netns_prefix());
        assert_ne!(p.blank(1).host_if, q.blank(1).host_if);
        // Nor collide with a sandbox's own names.
        assert!(!p.blank(1).host_if.starts_with("sst"));
    }

    #[test]
    fn a_claimed_namespace_ends_up_as_setup_builds_it() {
        let c = NetConfig::default();
        let s = c.slot("sb-claimed", 5).unwrap();
        let b = pool("/w").blank(3);
        let (host, inside) = claim_scripts(&b, &s);
        assert_eq!(
            host,
            format!(
                "link set dev {} name {}\naddr add 10.86.0.21/30 dev {}\nlink set dev {} up\n",
                b.host_if, s.host_if, s.host_if, s.host_if
            )
        );
        // Renamed before it is brought up: the kernel refuses to rename a
        // running interface.
        let lines: Vec<&str> = inside.lines().collect();
        assert_eq!(
            lines[0],
            format!("link set dev {} name {}", b.sandbox_if, s.sandbox_if)
        );
        assert_eq!(
            lines[1],
            format!("addr add 10.86.0.22/30 dev {}", s.sandbox_if)
        );
        assert_eq!(lines[2], format!("link set dev {} up", s.sandbox_if));
        assert_eq!(lines[3], "route add default via 10.86.0.21");
    }

    #[test]
    fn the_pool_waits_out_a_claim_before_refilling() {
        let p = pool("/w");
        assert_eq!(p.quiet_left(), None, "nothing taken, nothing to wait for");
        p.take();
        let left = p.quiet_left().expect("just taken");
        assert!(left <= REFILL_QUIET && left > REFILL_QUIET / 2);
        p.state.lock().unwrap().last_take =
            Some(std::time::Instant::now() - REFILL_QUIET - std::time::Duration::from_millis(1));
        assert_eq!(p.quiet_left(), None, "quiet long enough");
    }

    #[test]
    fn an_empty_pool_hands_out_nothing() {
        let p = pool("/w");
        assert!(p.take().is_none());
        p.state.lock().unwrap().ready.push(p.blank(1));
        assert_eq!(p.take(), Some(p.blank(1)));
        assert!(p.take().is_none());
    }

    #[test]
    fn a_smaller_range_holds_fewer_sandboxes() {
        let c = NetConfig {
            base: Ipv4Addr::new(10, 200, 0, 0),
            prefix: 24,
        };
        assert!(c.slot("sb", 63).is_ok(), "a /24 holds 64 slots");
        assert!(c.slot("sb", 64).is_err());
        assert_eq!(c.subnet(), "10.200.0.0/24");
    }
}
