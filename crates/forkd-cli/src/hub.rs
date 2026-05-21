//! Snapshot Hub — pack/unpack/pull/list for parent snapshots.
//!
//! Pack format v1 (`.forkd-snapshot.tar.zst`):
//!
//! ```text
//! tar.zst archive containing:
//!   manifest.toml      — name, format version, file sha256s, optional parent_tag
//!   memory.bin         — CoW source for child mmap (LARGEST file)
//!   vmstate            — Firecracker vCPU + device state
//!   snapshot.json      — forkd metadata (volumes, etc.)
//!   rootfs.ext4        — block device for child overlays
//! ```
//!
//! The manifest's `forkd_pack_version` lets us evolve the format
//! without breaking older clients — bump it on incompatible changes.
//! `parent_tag` is reserved for the M2.1 diff-snapshot chain work
//! (currently always None).

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;
use std::time::Instant;

/// Current pack format version. Bump on incompatible changes.
pub const PACK_FORMAT_VERSION: u32 = 1;

/// `manifest.toml` shipped at the root of every pack.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    /// Format version; reject on mismatch.
    pub forkd_pack_version: u32,
    /// Owner-qualified tag, e.g. "deeplethe/python-numpy".
    pub tag: String,
    /// Optional human description shown by `forkd images list`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Upstream base image, e.g. "python:3.12-slim". Informational.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_image: Option<String>,
    /// RFC3339 timestamp of when the pack was created.
    pub created_at: String,
    /// forkd version that wrote this pack.
    pub forkd_version: String,
    /// If set, this is a diff snapshot rooted at <parent_tag>. Reserved for M2.1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_tag: Option<String>,
    /// Per-file metadata (path inside the pack, size, sha256).
    pub files: Vec<FileEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

/// Files that make up a snapshot directory. Order matters: largest last
/// so progress reporting reads roughly increasing.
const SNAPSHOT_FILES: &[&str] = &["snapshot.json", "vmstate", "rootfs.ext4", "memory.bin"];

/// Pack a local snapshot directory into a single `.forkd-snapshot.tar.zst`.
///
/// Skips files that don't exist (older snapshots may not have all of
/// these), but requires at least `vmstate` + `memory.bin` since those
/// are the minimum for a usable snapshot.
pub fn pack(
    tag: &str,
    description: Option<String>,
    base_image: Option<String>,
    snap_dir: &Path,
    out_path: &Path,
) -> Result<Manifest> {
    if !snap_dir.join("vmstate").exists() {
        bail!(
            "snapshot directory {} has no `vmstate`; nothing to pack",
            snap_dir.display()
        );
    }
    if !snap_dir.join("memory.bin").exists() {
        bail!(
            "snapshot directory {} has no `memory.bin`; nothing to pack",
            snap_dir.display()
        );
    }

    let mut files: Vec<FileEntry> = Vec::new();
    for name in SNAPSHOT_FILES {
        let path = snap_dir.join(name);
        if !path.exists() {
            continue;
        }
        let meta = std::fs::metadata(&path).with_context(|| format!("stat {}", path.display()))?;
        let sha = sha256_file(&path)?;
        files.push(FileEntry {
            path: (*name).to_string(),
            size: meta.len(),
            sha256: sha,
        });
    }

    let manifest = Manifest {
        forkd_pack_version: PACK_FORMAT_VERSION,
        tag: tag.to_string(),
        description,
        base_image,
        created_at: chrono_like_now(),
        forkd_version: env!("CARGO_PKG_VERSION").to_string(),
        parent_tag: None,
        files: files.clone(),
    };

    // Write manifest as a temp file we'll include in the tar. Doing this
    // via tar::Builder::append_data() would also work but file-based
    // is simpler when computing the in-archive header.
    let manifest_toml = toml::to_string_pretty(&manifest).context("serialize manifest")?;

    let out_file =
        File::create(out_path).with_context(|| format!("create {}", out_path.display()))?;
    let zstd_writer = zstd::Encoder::new(out_file, 0)
        .context("init zstd encoder")?
        .auto_finish();
    let mut tar = tar::Builder::new(zstd_writer);

    // manifest.toml first so unpackers can read it without scanning the
    // whole archive.
    let manifest_bytes = manifest_toml.as_bytes();
    let mut header = tar::Header::new_gnu();
    header.set_size(manifest_bytes.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(unix_secs_now());
    header.set_cksum();
    tar.append_data(&mut header, "manifest.toml", manifest_bytes)
        .context("append manifest.toml")?;

    for entry in &files {
        let path = snap_dir.join(&entry.path);
        let mut f = File::open(&path).with_context(|| format!("open {}", path.display()))?;
        tar.append_file(&entry.path, &mut f)
            .with_context(|| format!("append {} to tar", entry.path))?;
    }

    tar.finish().context("tar finish")?;
    // zstd encoder finishes on drop via auto_finish.
    Ok(manifest)
}

/// Unpack a `.forkd-snapshot.tar.zst` into `dest_dir`. Verifies the
/// manifest's pack-format version and each file's sha256.
pub fn unpack(pack_path: &Path, dest_dir: &Path) -> Result<Manifest> {
    std::fs::create_dir_all(dest_dir).with_context(|| format!("create {}", dest_dir.display()))?;

    let in_file = File::open(pack_path).with_context(|| format!("open {}", pack_path.display()))?;
    let zstd_reader = zstd::Decoder::new(in_file).with_context(|| {
        format!(
            "initialize zstd decoder on {} (is this a .tar.zst file?)",
            pack_path.display()
        )
    })?;
    let mut tar = tar::Archive::new(zstd_reader);

    let mut manifest: Option<Manifest> = None;
    let mut extracted_files: Vec<String> = Vec::new();

    for entry in tar
        .entries()
        .context("open the tar archive inside the pack")?
    {
        let mut entry = entry.with_context(|| {
            format!(
                "read an entry from {} — pack may be corrupted, truncated, \
                 or not a forkd snapshot pack",
                pack_path.display()
            )
        })?;
        let path = entry
            .path()
            .context("decode an entry's path header (malformed tar?)")?
            .into_owned();
        let name = path.to_string_lossy();

        // Reject any path that escapes dest_dir.
        if name.contains("..") || name.starts_with('/') {
            bail!("malformed pack: refusing entry with traversal path: {name}");
        }

        if name == "manifest.toml" {
            let mut buf = String::new();
            entry
                .read_to_string(&mut buf)
                .context("read the manifest.toml body from the pack")?;
            let m: Manifest = toml::from_str(&buf).with_context(|| {
                format!(
                    "parse manifest.toml inside the pack (it is {} bytes, \
                     starts with {:?})",
                    buf.len(),
                    buf.chars().take(40).collect::<String>()
                )
            })?;
            if m.forkd_pack_version > PACK_FORMAT_VERSION {
                bail!(
                    "pack format version {} is newer than this forkd supports ({}). \
                     Upgrade forkd or repack with --pack-version {}.",
                    m.forkd_pack_version,
                    PACK_FORMAT_VERSION,
                    PACK_FORMAT_VERSION
                );
            }
            manifest = Some(m);
            continue;
        }

        let dest = dest_dir.join(&*name);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let mut out = File::create(&dest).with_context(|| format!("create {}", dest.display()))?;
        io::copy(&mut entry, &mut out).with_context(|| format!("write {}", dest.display()))?;
        extracted_files.push(name.into_owned());
    }

    let manifest = manifest.ok_or_else(|| {
        anyhow::anyhow!(
            "pack is missing manifest.toml — not a valid forkd snapshot pack \
             (extracted {} entries before noticing)",
            extracted_files.len()
        )
    })?;

    // Verify every file the manifest declared is present and matches the
    // recorded sha256. We do this *after* extraction so partial extracts
    // are visible for debugging if something goes wrong.
    for entry in &manifest.files {
        let path = dest_dir.join(&entry.path);
        let actual = sha256_file(&path).with_context(|| {
            format!(
                "hash {} for integrity check (declared in manifest)",
                path.display()
            )
        })?;
        if actual != entry.sha256 {
            bail!(
                "integrity check failed for {}: file sha256={} but manifest says {}. \
                 The pack is corrupted (truncated download? bit rot? tampered?).",
                entry.path,
                actual,
                entry.sha256
            );
        }
    }

    Ok(manifest)
}

/// Stream a remote URL to a local file with a periodic progress line.
///
/// Used by `forkd pull`. Public so callers can reuse for diff snapshot
/// chain pulls later.
pub fn download(url: &str, out_path: &Path) -> Result<u64> {
    eprintln!("==> GET {url}");
    let resp = ureq::get(url).call().with_context(|| {
        format!(
            "HTTP GET failed for {url} \
             (check the URL, DNS, and whether the server is reachable)"
        )
    })?;
    if resp.status() != 200 {
        bail!(
            "HTTP GET {url} returned status {} ({}). \
             Expected 200 for the snapshot pack — \
             404 usually means the tag isn't published yet, \
             403 means the bucket is private.",
            resp.status(),
            resp.status_text(),
        );
    }
    let content_length: Option<u64> = resp.header("Content-Length").and_then(|s| s.parse().ok());

    let mut out =
        File::create(out_path).with_context(|| format!("create {}", out_path.display()))?;

    let started = Instant::now();
    let mut written: u64 = 0;
    let mut last_log = Instant::now();
    let mut buf = vec![0u8; 64 * 1024];
    let mut reader = resp.into_reader();

    loop {
        let n = reader.read(&mut buf).context("read response body")?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n]).context("write to output")?;
        written += n as u64;
        if last_log.elapsed().as_secs() >= 1 {
            log_progress(written, content_length, started);
            last_log = Instant::now();
        }
    }
    log_progress(written, content_length, started);
    eprintln!();
    Ok(written)
}

/// HTTP PUT a local pack file to a URL (e.g. a presigned PUT for S3/R2).
///
/// Used by `forkd push`. The server is expected to accept the body
/// as-is — we don't set Content-Type because S3/R2 presigned PUTs
/// already encode it. Streams the body so memory.bin-sized packs
/// don't materialise in RAM.
pub fn upload(pack_path: &Path, url: &str) -> Result<u64> {
    let meta =
        std::fs::metadata(pack_path).with_context(|| format!("stat {}", pack_path.display()))?;
    let size = meta.len();
    eprintln!(
        "==> PUT {url}  ({} from {})",
        human_bytes(size),
        pack_path.display()
    );
    let file = File::open(pack_path).with_context(|| format!("open {}", pack_path.display()))?;

    let started = Instant::now();
    // Wrap the file in a reader that logs progress as bytes are consumed
    // by ureq's body upload. ureq drains the reader to send the body.
    let reader = ProgressReader::new(file, size, started);
    let resp = ureq::put(url)
        .set("Content-Length", &size.to_string())
        .send(reader)
        .with_context(|| {
            format!(
                "HTTP PUT failed for {url} \
                 (check the presigned URL hasn't expired, and the server is reachable)"
            )
        })?;
    eprintln!();
    if !(200..300).contains(&resp.status()) {
        bail!(
            "HTTP PUT {url} returned status {} ({}). \
             For presigned URLs: 403 typically means the URL signature expired, \
             400 often means a header mismatch (we set Content-Length but no Content-Type).",
            resp.status(),
            resp.status_text(),
        );
    }
    Ok(size)
}

/// Reader wrapper that emits a periodic upload progress line. Public
/// only inside the crate; `Read` impl strictly forwards to the wrapped
/// reader and updates the progress accumulator.
struct ProgressReader<R: Read> {
    inner: R,
    total: u64,
    written: u64,
    started: Instant,
    last_log: Instant,
}

impl<R: Read> ProgressReader<R> {
    fn new(inner: R, total: u64, started: Instant) -> Self {
        Self {
            inner,
            total,
            written: 0,
            started,
            last_log: started,
        }
    }
}

impl<R: Read> Read for ProgressReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.written += n as u64;
        if self.last_log.elapsed().as_secs() >= 1 {
            log_progress(self.written, Some(self.total), self.started);
            self.last_log = Instant::now();
        }
        Ok(n)
    }
}

fn log_progress(written: u64, total: Option<u64>, started: Instant) {
    let mb = (written as f64) / 1024.0 / 1024.0;
    let secs = started.elapsed().as_secs_f64().max(0.001);
    let rate = mb / secs;
    match total {
        Some(t) if t > 0 => {
            let total_mb = (t as f64) / 1024.0 / 1024.0;
            let pct = (written as f64) / (t as f64) * 100.0;
            eprint!(
                "\r    {:>6.1} / {:>6.1} MiB ({:>5.1}% · {:>5.1} MiB/s) ",
                mb, total_mb, pct, rate
            );
        }
        _ => {
            eprint!("\r    {:>6.1} MiB ({:>5.1} MiB/s) ", mb, rate);
        }
    }
    let _ = io::stderr().flush();
}

/// SHA-256 a file by streaming 64 KiB chunks (avoids loading multi-GiB
/// memory.bin into RAM).
pub fn sha256_file(path: &Path) -> Result<String> {
    let mut f = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f
            .read(&mut buf)
            .with_context(|| format!("read {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{:02x}", b);
    }
    s
}

/// Render a list-of-local-snapshots line for `forkd images list`. Walks
/// `snapshots/` under the data dir and reports tag + total size +
/// memory.bin size + dir mtime.
pub struct LocalSnapshotInfo {
    pub tag: String,
    pub total_bytes: u64,
    pub memory_bytes: u64,
    pub has_rootfs: bool,
    /// Unix seconds. Best-effort: directory mtime; 0 if unreadable.
    pub created_at_unix: u64,
}

pub fn list_local(snapshots_root: &Path) -> Result<Vec<LocalSnapshotInfo>> {
    let mut out = Vec::new();
    if !snapshots_root.exists() {
        return Ok(out);
    }
    for entry in std::fs::read_dir(snapshots_root)
        .with_context(|| format!("read {}", snapshots_root.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let tag = entry.file_name().to_string_lossy().into_owned();
        let dir = entry.path();
        let mut total: u64 = 0;
        let mut memory: u64 = 0;
        let mut has_rootfs = false;
        for name in SNAPSHOT_FILES {
            let p = dir.join(name);
            if let Ok(m) = std::fs::metadata(&p) {
                total += m.len();
                if *name == "rootfs.ext4" {
                    has_rootfs = true;
                } else if *name == "memory.bin" {
                    memory = m.len();
                }
            }
        }
        let created_at_unix = std::fs::metadata(&dir)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        out.push(LocalSnapshotInfo {
            tag,
            total_bytes: total,
            memory_bytes: memory,
            has_rootfs,
            created_at_unix,
        });
    }
    // Most recent first; ties broken by tag.
    out.sort_by(|a, b| {
        b.created_at_unix
            .cmp(&a.created_at_unix)
            .then_with(|| a.tag.cmp(&b.tag))
    });
    Ok(out)
}

/// Format a unix timestamp as a human-readable "age" relative to now.
/// Examples: "3m ago", "12h ago", "2d ago", "—" if unknown.
pub fn human_age(created_at_unix: u64) -> String {
    if created_at_unix == 0 {
        return "—".to_string();
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let dt = now.saturating_sub(created_at_unix);
    if dt < 60 {
        format!("{dt}s ago")
    } else if dt < 3600 {
        format!("{}m ago", dt / 60)
    } else if dt < 86400 {
        format!("{}h ago", dt / 3600)
    } else if dt < 86400 * 30 {
        format!("{}d ago", dt / 86400)
    } else {
        format!("{}mo ago", dt / 86400 / 30)
    }
}

/// Pretty MiB / GiB formatter for `forkd images list`.
pub fn human_bytes(n: u64) -> String {
    let n = n as f64;
    if n >= 1024.0 * 1024.0 * 1024.0 {
        format!("{:.2} GiB", n / 1024.0 / 1024.0 / 1024.0)
    } else if n >= 1024.0 * 1024.0 {
        format!("{:.1} MiB", n / 1024.0 / 1024.0)
    } else {
        format!("{:.0} KiB", n / 1024.0)
    }
}

/// Tiny non-`chrono` RFC3339 stamper to avoid pulling chrono in just
/// for one timestamp. Resolution is seconds, UTC.
fn chrono_like_now() -> String {
    let secs = unix_secs_now();
    let (year, month, day, hour, min, sec) = epoch_to_ymd_hms(secs);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year, month, day, hour, min, sec
    )
}

fn unix_secs_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Days-from-epoch → calendar (Gregorian). Stripped from RFC 3339 spec
/// to avoid the chrono dep.
fn epoch_to_ymd_hms(secs: u64) -> (u32, u32, u32, u32, u32, u32) {
    let s = (secs % 60) as u32;
    let m = ((secs / 60) % 60) as u32;
    let h = ((secs / 3600) % 24) as u32;
    let mut days = (secs / 86400) as i64;

    // Shift so day 0 = 0000-03-01 (lets Feb have predictable length).
    days += 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let doe = (days - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m_real = if mp < 10 { mp + 3 } else { mp - 9 };
    let y_real = if m_real <= 2 { y + 1 } else { y };
    (y_real as u32, m_real, d, h, m, s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_format_version_is_1() {
        assert_eq!(PACK_FORMAT_VERSION, 1);
    }

    #[test]
    fn human_bytes_formats() {
        assert_eq!(human_bytes(0), "0 KiB");
        assert_eq!(human_bytes(1024), "1 KiB");
        assert_eq!(human_bytes(2 * 1024 * 1024), "2.0 MiB");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024), "3.00 GiB");
    }

    #[test]
    fn epoch_basic() {
        // 2020-01-01T00:00:00Z = 1577836800
        let (y, m, d, h, mm, s) = epoch_to_ymd_hms(1_577_836_800);
        assert_eq!((y, m, d, h, mm, s), (2020, 1, 1, 0, 0, 0));
    }

    #[test]
    fn manifest_roundtrip() {
        let m = Manifest {
            forkd_pack_version: 1,
            tag: "deeplethe/python-numpy".into(),
            description: Some("Python + numpy".into()),
            base_image: Some("python:3.12-slim".into()),
            created_at: "2026-05-13T20:00:00Z".into(),
            forkd_version: "0.1.2".into(),
            parent_tag: None,
            files: vec![FileEntry {
                path: "memory.bin".into(),
                size: 1024,
                sha256: "abc".into(),
            }],
        };
        let s = toml::to_string_pretty(&m).unwrap();
        let m2: Manifest = toml::from_str(&s).unwrap();
        assert_eq!(m.tag, m2.tag);
        assert_eq!(m.files.len(), m2.files.len());
        assert_eq!(m.files[0].sha256, m2.files[0].sha256);
    }

    #[test]
    fn pack_unpack_roundtrip() {
        // Synthesize a fake snapshot dir and roundtrip it.
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir(&src).unwrap();
        std::fs::write(src.join("vmstate"), b"vmstate-bytes").unwrap();
        std::fs::write(src.join("memory.bin"), vec![0u8; 4096]).unwrap();
        std::fs::write(
            src.join("snapshot.json"),
            br#"{"vmstate":"x","memory":"y","volumes":[]}"#,
        )
        .unwrap();

        let pack_out = tmp.path().join("out.tar.zst");
        let m = pack("test/demo", Some("smoke".into()), None, &src, &pack_out).expect("pack");
        assert_eq!(m.tag, "test/demo");
        assert!(pack_out.exists());
        assert!(pack_out.metadata().unwrap().len() > 0);

        let dst = tmp.path().join("dst");
        let m2 = unpack(&pack_out, &dst).expect("unpack");
        assert_eq!(m2.tag, "test/demo");
        assert_eq!(
            std::fs::read(dst.join("vmstate")).unwrap(),
            b"vmstate-bytes"
        );
        assert_eq!(std::fs::read(dst.join("memory.bin")).unwrap().len(), 4096);
    }

    // Path-traversal rejection is intentionally not unit-tested here:
    // the `tar` crate's `Builder::append_data()` refuses to *write* an
    // entry with `..` segments, so we can't craft a malicious archive
    // via the safe API. The check in `unpack()` is defense-in-depth
    // against tars crafted by other tooling (raw bytes, `tar(1)`,
    // language-mismatched implementations) — which would require a
    // fixture file or hand-rolled header bytes to test.
}
