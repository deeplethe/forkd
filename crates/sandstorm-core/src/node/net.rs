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
pub fn setup(cfg: &NetConfig, id: &str, index: u32) -> Result<Slot> {
    let s = cfg.slot(id, index)?;

    if !std::path::Path::new(&format!("/var/run/netns/{}", s.netns)).exists() {
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
