//! `forkd doctor` — diagnose host setup, surface fix hints.
//!
//! Quick checklist of "things that must be true before forkd can fork
//! a microVM on this host". Each check returns one of:
//!   ✓ PASS  — green, all good
//!   ⚠ WARN  — yellow, works but you'll hit it later
//!   ✗ FAIL  — red, blocking; emit a fix hint
//!
//! The point is to compress the "I ran `forkd fork` and it errored
//! mysteriously" debugging loop into a single command. Designed to be
//! safe to run unprivileged (skips checks that need root and notes so).

use std::path::Path;
use std::process::Command;
use std::time::Duration;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Pass,
    Warn,
    Fail,
    Skip,
}

impl Status {
    fn glyph(self) -> &'static str {
        match self {
            Status::Pass => "\x1b[32m✓\x1b[0m",
            Status::Warn => "\x1b[33m⚠\x1b[0m",
            Status::Fail => "\x1b[31m✗\x1b[0m",
            Status::Skip => "\x1b[90m·\x1b[0m",
        }
    }
}

struct Check {
    name: &'static str,
    status: Status,
    detail: String,
    hint: Option<String>,
}

impl Check {
    fn pass(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: Status::Pass,
            detail: detail.into(),
            hint: None,
        }
    }
    fn warn(name: &'static str, detail: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            name,
            status: Status::Warn,
            detail: detail.into(),
            hint: Some(hint.into()),
        }
    }
    fn fail(name: &'static str, detail: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            name,
            status: Status::Fail,
            detail: detail.into(),
            hint: Some(hint.into()),
        }
    }
    #[allow(dead_code)] // used on non-Linux builds via cfg-gated checks
    fn skip(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: Status::Skip,
            detail: detail.into(),
            hint: None,
        }
    }
}

pub fn run(daemon_url: &str, daemon_token: Option<String>) -> anyhow::Result<()> {
    let checks: Vec<Check> = vec![
        check_platform(),
        check_hw_virt(),
        check_kvm(),
        check_cgroup_v2(),
        check_ip_forward(),
        check_tap_device("forkd-tap0"),
        check_netns_count(),
        check_firecracker_binary(),
        check_firecracker_version(),
        check_kernel_image(),
        check_snapshot_dir(),
        check_snapshot_dir_space(),
        check_docker_daemon(),
        check_daemon(daemon_url, daemon_token.as_deref()),
        // v0.4 live-fork prerequisites. WARN rather than FAIL so that
        // users who only want v0.3 Diff BRANCH don't get a red doctor
        // exit on a perfectly capable host that just hasn't enabled
        // unprivileged_userfaultfd / runs an older kernel.
        check_uffd_wp(),
        check_memfd_create(),
    ];

    print_report(&checks);
    let any_fail = checks.iter().any(|c| c.status == Status::Fail);
    if any_fail {
        anyhow::bail!("doctor found blocking issues — see above");
    }
    Ok(())
}

fn print_report(checks: &[Check]) {
    let max_name = checks.iter().map(|c| c.name.len()).max().unwrap_or(0);
    for c in checks {
        println!(
            "  {}  {:<width$}  {}",
            c.status.glyph(),
            c.name,
            c.detail,
            width = max_name
        );
        if let Some(h) = &c.hint {
            println!(
                "       {:<width$}    \x1b[90m→ {}\x1b[0m",
                "",
                h,
                width = max_name
            );
        }
    }
    let pass = checks.iter().filter(|c| c.status == Status::Pass).count();
    let warn = checks.iter().filter(|c| c.status == Status::Warn).count();
    let fail = checks.iter().filter(|c| c.status == Status::Fail).count();
    let skip = checks.iter().filter(|c| c.status == Status::Skip).count();
    println!();
    println!("  pass={pass}  warn={warn}  fail={fail}  skip={skip}");
}

// ----------------------------------------------------------------------
// Individual checks
// ----------------------------------------------------------------------

fn check_platform() -> Check {
    #[cfg(target_os = "linux")]
    {
        // Just stat /proc/version for the kernel.
        match std::fs::read_to_string("/proc/version") {
            Ok(v) => {
                let first = v.split_whitespace().take(3).collect::<Vec<_>>().join(" ");
                Check::pass("platform", first)
            }
            Err(e) => Check::warn(
                "platform",
                format!("read /proc/version: {e}"),
                "expected Linux",
            ),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        Check::fail("platform", "not Linux", "forkd requires Linux + KVM")
    }
}

fn check_kvm() -> Check {
    #[cfg(target_os = "linux")]
    {
        let dev = Path::new("/dev/kvm");
        if !dev.exists() {
            return Check::fail(
                "kvm",
                "/dev/kvm does not exist",
                "enable KVM (Intel: kvm_intel; AMD: kvm_amd) or run on bare metal",
            );
        }
        // Try to open it as the current user; need at least read perms.
        match std::fs::File::open(dev) {
            Ok(_) => Check::pass("kvm", "/dev/kvm OK"),
            Err(e) => Check::fail(
                "kvm",
                format!("/dev/kvm: {e}"),
                "add yourself to the 'kvm' group: sudo usermod -aG kvm $USER && newgrp kvm",
            ),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        Check::skip("kvm", "not Linux")
    }
}

fn check_cgroup_v2() -> Check {
    let unified = Path::new("/sys/fs/cgroup/cgroup.controllers");
    if unified.exists() {
        match std::fs::read_to_string(unified) {
            Ok(c) => Check::pass("cgroup v2", c.trim().to_string()),
            Err(_) => Check::warn(
                "cgroup v2",
                "/sys/fs/cgroup/cgroup.controllers unreadable",
                "memory.max enforcement may not work",
            ),
        }
    } else {
        Check::warn(
            "cgroup v2",
            "no unified hierarchy mounted",
            "forkd works but memory_limit_mib is silently ignored",
        )
    }
}

fn check_ip_forward() -> Check {
    match std::fs::read_to_string("/proc/sys/net/ipv4/ip_forward") {
        Ok(v) if v.trim() == "1" => Check::pass("ip_forward", "1 (forwarding enabled)"),
        Ok(v) => Check::fail(
            "ip_forward",
            v.trim().to_string(),
            "sudo sysctl -w net.ipv4.ip_forward=1 — or rerun scripts/host-tap.sh",
        ),
        Err(e) => Check::warn("ip_forward", format!("{e}"), "expected on Linux"),
    }
}

fn check_tap_device(tap: &'static str) -> Check {
    // Best-effort: read /sys/class/net/<tap>/{operstate,flags}. A TAP
    // device's natural idle state is admin-UP + operstate=DOWN
    // (NO-CARRIER), because nothing has it open as the TUN endpoint.
    // operstate flips to "up" or "unknown" only when a peer attaches
    // (firecracker, vhost, etc.). So PASS as long as the admin flag
    // is UP — distinguishing "tap fine, just idle" from "tap admin-
    // disabled" or "tap missing".
    let sys_path = format!("/sys/class/net/{tap}");
    if !Path::new(&sys_path).exists() {
        return Check::fail(
            "tap device",
            format!("{tap} not present"),
            "sudo bash scripts/host-tap.sh",
        );
    }
    let oper = std::fs::read_to_string(format!("{sys_path}/operstate"))
        .unwrap_or_else(|_| "?".to_string())
        .trim()
        .to_string();
    // /sys/class/net/<dev>/flags is a hex string like "0x1003".
    // IFF_UP = 0x1. If admin says UP, the device is configured and the
    // operstate ("down" with no carrier) is just "no peer attached".
    let admin_up = std::fs::read_to_string(format!("{sys_path}/flags"))
        .ok()
        .and_then(|s| {
            let s = s.trim();
            let s = s.strip_prefix("0x").unwrap_or(s);
            u64::from_str_radix(s, 16).ok()
        })
        .map(|f| f & 0x1 != 0)
        .unwrap_or(false);
    if admin_up {
        // "up" / "unknown" — peer attached, link active.
        // "down" + admin-UP — idle, peer not yet attached (normal).
        let note = if oper == "up" || oper == "unknown" {
            format!("{tap} (up, peer attached)")
        } else {
            format!("{tap} (admin-UP, idle)")
        };
        Check::pass("tap device", note)
    } else {
        Check::warn(
            "tap device",
            format!("{tap} ({oper})"),
            "sudo ip link set forkd-tap0 up — or rerun scripts/host-tap.sh",
        )
    }
}

fn check_netns_count() -> Check {
    let nsdir = Path::new("/var/run/netns");
    if !nsdir.exists() {
        return Check::warn(
            "per-child netns",
            "no /var/run/netns",
            "needed for fanout >1: sudo bash scripts/netns-setup.sh N",
        );
    }
    let mut count = 0usize;
    if let Ok(rd) = std::fs::read_dir(nsdir) {
        for e in rd.flatten() {
            if e.file_name().to_string_lossy().starts_with("forkd-child-") {
                count += 1;
            }
        }
    }
    if count == 0 {
        Check::warn(
            "per-child netns",
            "no forkd-child-* netns present",
            "sudo bash scripts/netns-setup.sh N (for fanout > 1)",
        )
    } else {
        Check::pass("per-child netns", format!("{count} provisioned"))
    }
}

fn check_firecracker_binary() -> Check {
    // Look in $PATH, then check the canonical location forkd-vmm
    // expects (/usr/local/bin or ~/.local/bin).
    for candidate in ["firecracker"] {
        if let Ok(out) = Command::new("which").arg(candidate).output() {
            if out.status.success() {
                let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !p.is_empty() {
                    return Check::pass("firecracker", p);
                }
            }
        }
    }
    Check::fail(
        "firecracker",
        "not on PATH",
        "install via scripts/setup-host.sh, or curl from https://github.com/firecracker-microvm/firecracker/releases",
    )
}

fn check_firecracker_version() -> Check {
    // `firecracker --version` first line is like "Firecracker v1.10.1"
    let out = match Command::new("firecracker").arg("--version").output() {
        Ok(o) => o,
        Err(_) => return Check::skip("firecracker version", "binary not on PATH (see above)"),
    };
    let first = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .unwrap_or("")
        .to_string();
    if first.is_empty() {
        return Check::warn(
            "firecracker version",
            "couldn't parse --version output",
            "unexpected; check binary manually",
        );
    }
    // Pull "v1.10.1" out of the line.
    let ver = first
        .split_whitespace()
        .find(|t| t.starts_with('v') && t.contains('.'))
        .unwrap_or("?");
    // Diff snapshots need >=1.5; we recommend >=1.10 for v0.3 forkd.
    let major_minor: Option<(u32, u32)> = ver
        .trim_start_matches('v')
        .split('.')
        .take(2)
        .collect::<Vec<_>>()
        .try_into()
        .ok()
        .and_then(|p: [&str; 2]| Some((p[0].parse().ok()?, p[1].parse().ok()?)));
    match major_minor {
        Some((maj, min)) if maj > 1 || (maj == 1 && min >= 10) => {
            Check::pass("firecracker version", ver.to_string())
        }
        Some((maj, min)) if maj == 1 && min >= 5 => Check::warn(
            "firecracker version",
            format!("{ver} works but pre-1.10"),
            "upgrade to >=1.10 for the snapshot path forkd v0.3 was tested against",
        ),
        Some(_) => Check::fail(
            "firecracker version",
            format!("{ver} too old (need >=1.5 for diff snapshots)"),
            "curl a recent build from https://github.com/firecracker-microvm/firecracker/releases",
        ),
        None => Check::warn(
            "firecracker version",
            format!("could not parse: {first}"),
            "expected 'Firecracker vMAJ.MIN.PATCH'",
        ),
    }
}

fn check_hw_virt() -> Check {
    // Quick check: /proc/cpuinfo has vmx (Intel) or svm (AMD) flag.
    #[cfg(target_os = "linux")]
    {
        let info = match std::fs::read_to_string("/proc/cpuinfo") {
            Ok(s) => s,
            Err(e) => {
                return Check::warn("hw virt", format!("{e}"), "expected /proc/cpuinfo on Linux")
            }
        };
        let flags_line = info.lines().find(|l| l.starts_with("flags")).unwrap_or("");
        if flags_line.contains(" vmx") || flags_line.contains(" svm") {
            let kind = if flags_line.contains(" vmx") {
                "vmx"
            } else {
                "svm"
            };
            Check::pass("hw virt", format!("{kind} (CPU supports virtualization)"))
        } else {
            Check::fail(
                "hw virt",
                "no vmx or svm flag in /proc/cpuinfo",
                "enable virtualization in BIOS/UEFI (Intel VT-x / AMD-V), or run on bare metal",
            )
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        Check::skip("hw virt", "not Linux")
    }
}

fn check_docker_daemon() -> Check {
    // Only relevant if the user wants `forkd from-image` / `forkd parent build`.
    // Check `docker info` — if Docker isn't installed, warn (not fail) since
    // forkd's other commands don't need it.
    let exists = Command::new("which")
        .arg("docker")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !exists {
        return Check::warn(
            "docker",
            "not installed",
            "needed only for `forkd from-image` / `forkd parent build`",
        );
    }
    match Command::new("docker").arg("info").output() {
        Ok(o) if o.status.success() => Check::pass("docker", "daemon reachable"),
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr);
            let hint = if err.contains("permission denied") {
                "sudo usermod -aG docker $USER && newgrp docker"
            } else {
                "sudo systemctl start docker"
            };
            Check::warn("docker", "daemon unreachable", hint)
        }
        Err(e) => Check::warn(
            "docker",
            format!("docker info errored: {e}"),
            "needed only for from-image / parent build",
        ),
    }
}

fn check_snapshot_dir_space() -> Check {
    #[cfg(unix)]
    {
        // Get available bytes on the filesystem that holds the snapshot dir.
        // statvfs(3) is cheap and Unix-portable.
        let home = std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from("/root"));
        let xdg = std::env::var_os("XDG_DATA_HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| home.join(".local/share"));
        let dir = xdg.join("forkd/snapshots");
        // statvfs needs a path that exists; walk up to an existing parent.
        let mut probe = dir.clone();
        while !probe.exists() {
            match probe.parent() {
                Some(p) if p.as_os_str() != probe.as_os_str() => probe = p.to_path_buf(),
                _ => return Check::warn("snapshot dir space", "no path to stat", ""),
            }
        }
        use std::os::unix::ffi::OsStrExt;
        let c_path = match std::ffi::CString::new(probe.as_os_str().as_bytes()) {
            Ok(c) => c,
            Err(_) => return Check::warn("snapshot dir space", "bad path", ""),
        };
        let mut buf: libc::statvfs = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut buf) };
        if rc != 0 {
            return Check::warn("snapshot dir space", "statvfs failed", "");
        }
        let avail_bytes = (buf.f_bavail as u64).saturating_mul(buf.f_frsize as u64);
        let avail_gib = avail_bytes as f64 / 1024.0 / 1024.0 / 1024.0;
        if avail_gib >= 5.0 {
            Check::pass(
                "snapshot dir space",
                format!("{avail_gib:.1} GiB free at {}", probe.display()),
            )
        } else if avail_gib >= 1.0 {
            Check::warn(
                "snapshot dir space",
                format!("{avail_gib:.1} GiB free"),
                "low — recommended ≥5 GiB for warmed parent rootfs + memory.bin",
            )
        } else {
            Check::fail(
                "snapshot dir space",
                format!("{avail_gib:.2} GiB free"),
                "clear space; one snapshot is typically 0.5-3 GiB",
            )
        }
    }
    #[cfg(not(unix))]
    {
        Check::skip("snapshot dir space", "not Unix")
    }
}

fn check_kernel_image() -> Check {
    // Look for a vmlinux in common spots.
    let candidates = [
        "./vmlinux-6.1.141",
        "./vmlinux",
        "/var/lib/forkd/kernels/vmlinux",
        "/usr/local/share/forkd/vmlinux",
    ];
    for c in candidates {
        if Path::new(c).exists() {
            return Check::pass("kernel image", c.to_string());
        }
    }
    Check::warn(
        "kernel image",
        "no vmlinux found in common locations",
        "sudo scripts/install-guest-kernel.sh   (or override the URL: \
         curl -L https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.13/x86_64/vmlinux-6.1.141 \
         -o /var/lib/forkd/kernels/vmlinux)",
    )
}

fn check_snapshot_dir() -> Check {
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/root"));
    let xdg = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share"));
    let dir = xdg.join("forkd/snapshots");
    if !dir.exists() {
        return Check::warn(
            "snapshot dir",
            format!("{} not created yet", dir.display()),
            "created lazily on first `forkd snapshot`",
        );
    }
    let mut count = 0usize;
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                count += 1;
            }
        }
    }
    Check::pass(
        "snapshot dir",
        format!("{} ({count} snapshots)", dir.display()),
    )
}

fn check_uffd_wp() -> Check {
    #[cfg(target_os = "linux")]
    {
        match forkd_uffd::probe::probe_uffd_wp() {
            Ok(()) => Check::pass("uffd_wp (v0.4 live BRANCH)", "supported"),
            Err(e) => {
                let msg = format!("{e:#}");
                // EPERM almost always means unprivileged_userfaultfd=0.
                let hint = if msg.contains("Operation not permitted") || msg.contains("EPERM") {
                    "sudo sysctl -w vm.unprivileged_userfaultfd=1 \
                     (or grant CAP_SYS_PTRACE to the daemon)"
                } else if msg.contains("Function not implemented") || msg.contains("ENOSYS") {
                    "kernel < 5.7 or built without userfaultfd; v0.4 live-fork \
                     unavailable. Diff (v0.3) and Full BRANCH still work."
                } else {
                    "v0.4 live BRANCH unavailable; Diff (v0.3) and Full still work. \
                     See docs/VENDORED-FIRECRACKER.md for the live-fork prerequisites."
                };
                Check::warn("uffd_wp (v0.4 live BRANCH)", msg, hint)
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        Check::skip("uffd_wp (v0.4 live BRANCH)", "not Linux")
    }
}

fn check_memfd_create() -> Check {
    #[cfg(target_os = "linux")]
    {
        match forkd_uffd::probe::probe_memfd_create() {
            Ok(()) => Check::pass("memfd_create (v0.4 live BRANCH)", "supported"),
            Err(e) => {
                let msg = format!("{e:#}");
                let hint = if msg.contains("Function not implemented") || msg.contains("ENOSYS") {
                    "kernel < 3.17 or seccomp blocking memfd_create; v0.4 \
                     live-fork unavailable."
                } else {
                    "v0.4 live BRANCH needs memfd_create; check container \
                     seccomp profile."
                };
                Check::warn("memfd_create (v0.4 live BRANCH)", msg, hint)
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        Check::skip("memfd_create (v0.4 live BRANCH)", "not Linux")
    }
}

fn check_daemon(daemon_url: &str, token: Option<&str>) -> Check {
    let url = format!("{}/v1/snapshots", daemon_url.trim_end_matches('/'));
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(2))
        .build();
    let mut req = agent.get(&url);
    if let Some(t) = token {
        req = req.set("Authorization", &format!("Bearer {t}"));
    }
    match req.call() {
        Ok(_) => Check::pass("daemon", format!("{daemon_url} responding")),
        Err(ureq::Error::Status(401, _)) => Check::fail(
            "daemon",
            format!("{daemon_url} HTTP 401"),
            "set FORKD_TOKEN to match the daemon's --token-file",
        ),
        Err(ureq::Error::Status(code, _)) => Check::warn(
            "daemon",
            format!("{daemon_url} HTTP {code}"),
            "daemon up but returning non-2xx",
        ),
        Err(e) => Check::warn(
            "daemon",
            format!("{daemon_url} unreachable: {e}"),
            "sudo systemctl start forkd-controller (or run it ad-hoc)",
        ),
    }
}
