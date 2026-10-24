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
//!   fkd<n>h  10.86.x.y+1/30  <-> fkd<n>s  10.86.x.y+2/30   (its netns)
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
            netns: format!("forkd-{id}"),
            host_if: format!("fkd{index}h"),
            sandbox_if: format!("fkd{index}s"),
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

pub fn teardown(cfg: &NetConfig, id: &str, index: u32) -> Result<()> {
    let s = cfg.slot(id, index)?;
    // Deleting the netns takes the peer with it; the host end has to go too.
    let _ = run("removing netns", &["ip", "netns", "del", &s.netns]);
    let _ = run("removing veth", &["ip", "link", "del", &s.host_if]);
    Ok(())
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
    tracing::info!(subnet = %subnet, "sandbox network rules in place");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
    #[test]
    fn the_namespace_is_named_for_the_sandbox() {
        let c = NetConfig::default();
        let s = c.slot("sb-deadbeef0123", 42).unwrap();
        assert_eq!(s.netns, "forkd-sb-deadbeef0123");
        assert_eq!(s.host_if, "fkd42h");
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
