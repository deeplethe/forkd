//! Building an OCI bundle for one sandbox.
//!
//! A template is a rootfs directory on the node plus a small JSON spec. The
//! node turns that into a full OCI spec; the template never carries the
//! security-relevant parts, so a template cannot widen what a sandbox may do.
//!
//! The per-child writable layer is an overlay, not a reflink copy. bm's root
//! filesystem is ext4, which has no reflink at all -- the earlier reflink
//! measurement had to build a loopback XFS to run. Overlay needs nothing from
//! the filesystem and is what container runtimes use for exactly this.
//!
//! The cgroup is named in the spec (`linux.cgroupsPath`), so runsc creates it
//! and the node knows the one path it must write to freeze the sandbox. That
//! is the proven arrangement from the tier probes.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Command as Exec;

use super::runtime::RuntimeConfig;

/// The part of a template an operator gets to choose.
#[derive(Debug, Clone, Deserialize)]
pub struct TemplateSpec {
    /// What to run. `{{PORT}}` in any argument is replaced with the
    /// sandbox's port, which is how a template stays port-agnostic.
    pub args: Vec<String>,
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    /// Host directories to bind read-only, so a template needs only a
    /// skeleton rootfs rather than a whole distribution.
    #[serde(default = "default_ro_binds")]
    pub ro_binds: Vec<String>,
    /// Hard memory ceiling. Without one, a single sandbox can take the node
    /// down, which the density work makes more likely rather than less.
    #[serde(default)]
    pub memory_max_mib: Option<u64>,
}

fn default_ro_binds() -> Vec<String> {
    ["/usr", "/lib", "/lib64", "/bin", "/sbin", "/etc"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// Standard runc default capability set, minus the ones a sandbox running
/// untrusted code has no business holding.
const CAPS: &[&str] = &[
    "CAP_CHOWN",
    "CAP_DAC_OVERRIDE",
    "CAP_FSETID",
    "CAP_FOWNER",
    "CAP_SETGID",
    "CAP_SETUID",
    "CAP_SETPCAP",
    "CAP_NET_BIND_SERVICE",
    "CAP_KILL",
];

pub struct Layout {
    pub bundle: PathBuf,
    pub merged: PathBuf,
    upper: PathBuf,
    work: PathBuf,
}

impl Layout {
    pub fn for_id(cfg: &RuntimeConfig, id: &str) -> Self {
        let layer = cfg.work_dir.join("layers").join(id);
        Self {
            bundle: cfg.work_dir.join("bundles").join(id),
            merged: layer.join("merged"),
            upper: layer.join("upper"),
            work: layer.join("work"),
        }
    }
}

/// Create the writable layer, mount it, and write the bundle's config.json.
/// Idempotent: re-running for a sandbox that already has a mounted layer
/// leaves it alone, which is what makes re-adoption after a restart free.
///
/// `restore_from`, when given, is an archive of a previously detached
/// sandbox's upper directory. It is unpacked BEFORE the overlay is mounted:
/// overlayfs does not guarantee that changes made to an upperdir while it is
/// mounted are visible, so unpacking afterwards would appear to work and
/// then silently lose the sandbox's state.
pub fn build(
    cfg: &RuntimeConfig,
    id: &str,
    template_rootfs: &Path,
    spec: &TemplateSpec,
    port: u16,
    volume: Option<&str>,
    restore_from: Option<&Path>,
) -> Result<Layout> {
    if !template_rootfs.is_dir() {
        bail!(
            "template rootfs {} is not a directory",
            template_rootfs.display()
        );
    }
    let l = Layout::for_id(cfg, id);
    for d in [&l.bundle, &l.merged, &l.upper, &l.work] {
        std::fs::create_dir_all(d).with_context(|| format!("creating {}", d.display()))?;
    }

    if !is_mounted(&l.merged)? {
        if let Some(arc) = restore_from {
            unpack_into(arc, &l.upper)
                .with_context(|| format!("restoring {id} from {}", arc.display()))?;
        }
        mount_overlay(template_rootfs, &l.upper, &l.work, &l.merged)?;
    } else if restore_from.is_some() {
        // The layer is already live, so the archive cannot be laid down
        // underneath it. Say so rather than appearing to restore.
        bail!("{id} already has a mounted layer; it cannot be restored over");
    }

    let config = oci_spec(id, &l.merged, spec, port, volume)?;
    std::fs::write(
        l.bundle.join("config.json"),
        serde_json::to_vec_pretty(&config)?,
    )
    .context("writing config.json")?;
    Ok(l)
}

/// Unmount the overlay, leaving the layer's directories in place.
///
/// Separate from `teardown` so detach can unmount BEFORE archiving the
/// upper directory. Reading a live upperdir is the same class of thing
/// overlayfs warns about for writing it, and there is no reason to: by
/// detach time the sandbox is already checkpointed and gone.
pub fn unmount(cfg: &RuntimeConfig, id: &str) -> Result<()> {
    let l = Layout::for_id(cfg, id);
    if !is_mounted(&l.merged).unwrap_or(false) {
        return Ok(());
    }
    match Exec::new("umount").arg(&l.merged).output() {
        Ok(o) if !o.status.success() => {
            bail!(
                "unmounting {} failed: {}",
                l.merged.display(),
                String::from_utf8_lossy(&o.stderr).trim()
            );
        }
        Err(e) => bail!("umount failed to run: {e}"),
        _ => Ok(()),
    }
}

/// Unmount and delete the layer. Called on destroy, and on detach once the
/// layer has been archived to the volume.
pub fn teardown(cfg: &RuntimeConfig, id: &str) -> Result<()> {
    let l = Layout::for_id(cfg, id);
    if is_mounted(&l.merged).unwrap_or(false) {
        let out = Exec::new("umount").arg(&l.merged).output();
        match out {
            Ok(o) if !o.status.success() => {
                // Lazy unmount rather than leaving a mount behind: the
                // directory is about to be removed either way.
                let _ = Exec::new("umount").arg("-l").arg(&l.merged).status();
            }
            Err(e) => tracing::warn!(error = %e, "umount failed to run"),
            _ => {}
        }
    }
    std::fs::remove_dir_all(cfg.work_dir.join("layers").join(id)).ok();
    std::fs::remove_dir_all(&l.bundle).ok();
    Ok(())
}

fn unpack_into(archive: &Path, dest: &Path) -> Result<()> {
    let status = Exec::new("sh")
        .arg("-c")
        .arg(format!(
            "zstd -dq -T0 -c {} | tar -C {} -xf -",
            shell_quote(&archive.to_string_lossy()),
            shell_quote(&dest.to_string_lossy())
        ))
        .status()
        .context("unpacking the archived layer")?;
    if !status.success() {
        bail!("unpacking {} failed with {status}", archive.display());
    }
    Ok(())
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn is_mounted(p: &Path) -> Result<bool> {
    let out = Exec::new("mountpoint").arg("-q").arg(p).status();
    Ok(matches!(out, Ok(s) if s.success()))
}

fn mount_overlay(lower: &Path, upper: &Path, work: &Path, merged: &Path) -> Result<()> {
    // Commas and colons separate overlay options, so a path containing one
    // would silently produce a different mount.
    for p in [lower, upper, work, merged] {
        let s = p.to_string_lossy();
        if s.contains(',') || s.contains(':') {
            bail!("path {s} contains a character overlay options cannot carry");
        }
    }
    let opts = format!(
        "lowerdir={},upperdir={},workdir={}",
        lower.display(),
        upper.display(),
        work.display()
    );
    let out = Exec::new("mount")
        .args(["-t", "overlay", "overlay", "-o"])
        .arg(&opts)
        .arg(merged)
        .output()
        .context("running mount -t overlay")?;
    if !out.status.success() {
        bail!(
            "overlay mount failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn oci_spec(
    id: &str,
    root: &Path,
    spec: &TemplateSpec,
    port: u16,
    volume: Option<&str>,
) -> Result<serde_json::Value> {
    let args: Vec<String> = spec
        .args
        .iter()
        .map(|a| a.replace("{{PORT}}", &port.to_string()))
        .collect();
    if args.is_empty() {
        bail!("template has no args, so there is nothing to run");
    }

    let mut env = vec![
        "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string(),
        "HOME=/root".to_string(),
        // Stops the sandbox writing .pyc files into the read-only binds on
        // every start, which shows up as noise and wasted work.
        "PYTHONDONTWRITEBYTECODE=1".to_string(),
    ];
    env.extend(spec.env.iter().cloned());

    let mut mounts = vec![
        json!({"destination": "/proc", "type": "proc", "source": "proc"}),
        json!({"destination": "/dev", "type": "tmpfs", "source": "tmpfs",
               "options": ["nosuid", "noexec", "strictatime", "mode=755", "size=65536k"]}),
        json!({"destination": "/dev/pts", "type": "devpts", "source": "devpts",
               "options": ["nosuid", "noexec", "newinstance", "ptmxmode=0666", "mode=0620"]}),
        json!({"destination": "/dev/shm", "type": "tmpfs", "source": "shm",
               "options": ["nosuid", "noexec", "nodev", "mode=1777", "size=65536k"]}),
        json!({"destination": "/sys", "type": "sysfs", "source": "sysfs",
               "options": ["nosuid", "noexec", "nodev", "ro"]}),
        json!({"destination": "/tmp", "type": "tmpfs", "source": "tmpfs",
               "options": ["nosuid", "nodev", "mode=1777", "size=262144k"]}),
    ];
    for b in &spec.ro_binds {
        if !Path::new(b).exists() {
            continue;
        }
        mounts.push(json!({
            "destination": b, "source": b, "type": "bind",
            "options": ["rbind", "ro", "nosuid", "nodev"]
        }));
    }
    // The volume is the only thing that survives the detached tier, so it is
    // the one writable mount that is not part of the throwaway layer.
    if let Some(v) = volume {
        std::fs::create_dir_all(v).with_context(|| format!("creating volume {v}"))?;
        mounts.push(json!({
            "destination": "/workspace", "source": v, "type": "bind",
            "options": ["rbind", "rw", "nosuid", "nodev"]
        }));
    }

    let mut resources = json!({});
    if let Some(mib) = spec.memory_max_mib {
        resources["memory"] = json!({ "limit": mib * 1024 * 1024 });
    }

    Ok(json!({
        "ociVersion": "1.0.2",
        "process": {
            "terminal": false,
            "user": { "uid": 0, "gid": 0 },
            "args": args,
            "env": env,
            "cwd": spec.cwd.clone().unwrap_or_else(|| "/".to_string()),
            "capabilities": {
                "bounding": CAPS, "effective": CAPS, "permitted": CAPS
            },
            "rlimits": [{ "type": "RLIMIT_NOFILE", "hard": 8192, "soft": 8192 }],
            "noNewPrivileges": true
        },
        "root": { "path": root.to_string_lossy(), "readonly": false },
        "hostname": id,
        "mounts": mounts,
        "linux": {
            // No network namespace: the sandbox shares the host's, which is
            // why it can bind 127.0.0.1:<port> and the node can reach it.
            // Real network isolation needs a netns plus a veth pair and is
            // tracked separately; this is the arrangement every tier
            // measurement was taken against.
            "namespaces": [
                {"type": "pid"}, {"type": "ipc"},
                {"type": "uts"}, {"type": "mount"}
            ],
            "cgroupsPath": format!("/forkd/{id}"),
            "resources": resources
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> TemplateSpec {
        TemplateSpec {
            args: vec![
                "/usr/bin/python3".into(),
                "/app/p.py".into(),
                "{{PORT}}".into(),
            ],
            env: vec!["FOO=bar".into()],
            cwd: None,
            ro_binds: default_ro_binds(),
            memory_max_mib: Some(256),
        }
    }

    #[test]
    fn the_port_reaches_the_command_line() {
        let s = oci_spec("sb1", Path::new("/r"), &spec(), 19900, None).unwrap();
        let args: Vec<String> = serde_json::from_value(s["process"]["args"].clone()).unwrap();
        assert_eq!(args[2], "19900", "{{PORT}} must be substituted");
    }

    #[test]
    fn the_cgroup_is_named_so_freeze_knows_where_to_write() {
        let s = oci_spec("sb1", Path::new("/r"), &spec(), 1, None).unwrap();
        assert_eq!(s["linux"]["cgroupsPath"], "/forkd/sb1");
    }

    #[test]
    fn a_memory_ceiling_reaches_the_spec() {
        let s = oci_spec("sb1", Path::new("/r"), &spec(), 1, None).unwrap();
        assert_eq!(
            s["linux"]["resources"]["memory"]["limit"],
            256 * 1024 * 1024
        );
    }

    /// A template must not be able to widen what a sandbox may do.
    #[test]
    fn the_security_relevant_parts_are_not_the_templates_to_choose() {
        let s = oci_spec("sb1", Path::new("/r"), &spec(), 1, None).unwrap();
        assert_eq!(s["process"]["noNewPrivileges"], true);
        let caps: Vec<String> =
            serde_json::from_value(s["process"]["capabilities"]["effective"].clone()).unwrap();
        for forbidden in [
            "CAP_SYS_ADMIN",
            "CAP_SYS_MODULE",
            "CAP_SYS_PTRACE",
            "CAP_NET_RAW",
        ] {
            assert!(
                !caps.contains(&forbidden.to_string()),
                "{forbidden} granted"
            );
        }
    }

    #[test]
    fn env_from_the_template_is_added_not_substituted() {
        let s = oci_spec("sb1", Path::new("/r"), &spec(), 1, None).unwrap();
        let env: Vec<String> = serde_json::from_value(s["process"]["env"].clone()).unwrap();
        assert!(env.iter().any(|e| e.starts_with("PATH=")), "PATH kept");
        assert!(env.contains(&"FOO=bar".to_string()), "template env added");
    }

    #[test]
    fn a_volume_is_the_one_writable_mount_outside_the_layer() {
        let td = tempfile::TempDir::new().unwrap();
        let vol = td.path().join("vol");
        let s = oci_spec(
            "sb1",
            Path::new("/r"),
            &spec(),
            1,
            Some(vol.to_str().unwrap()),
        )
        .unwrap();
        let mounts: Vec<serde_json::Value> = serde_json::from_value(s["mounts"].clone()).unwrap();
        let ws = mounts
            .iter()
            .find(|m| m["destination"] == "/workspace")
            .expect("volume mounted at /workspace");
        let opts: Vec<String> = serde_json::from_value(ws["options"].clone()).unwrap();
        assert!(opts.contains(&"rw".to_string()));
        // Every other bind stays read-only.
        for m in mounts.iter().filter(|m| m["type"] == "bind") {
            let o: Vec<String> = serde_json::from_value(m["options"].clone()).unwrap();
            if m["destination"] != "/workspace" {
                assert!(
                    o.contains(&"ro".to_string()),
                    "{:?} is not ro",
                    m["destination"]
                );
            }
        }
    }

    #[test]
    fn a_template_with_nothing_to_run_is_refused() {
        let mut s = spec();
        s.args.clear();
        assert!(oci_spec("sb1", Path::new("/r"), &s, 1, None).is_err());
    }

    /// Overlay options are comma and colon separated, so a path carrying
    /// one would quietly produce a different mount.
    #[test]
    fn overlay_refuses_a_path_it_cannot_express() {
        let bad = Path::new("/var/lib/forkd/a,b");
        let e = mount_overlay(bad, bad, bad, bad).unwrap_err().to_string();
        assert!(e.contains("cannot carry"), "got: {e}");
    }
}
