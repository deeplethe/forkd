//! Snapshot Hub — pack/unpack/pull/list for parent snapshots.
//!
//! ## Pack format v1 (single snapshot, pre-v0.5)
//!
//! ```text
//! tar.zst archive:
//!   manifest.toml      — name, format version, file sha256s, optional parent_tag
//!   memory.bin         — CoW source for child mmap (LARGEST file)
//!   vmstate            — Firecracker vCPU + device state
//!   snapshot.json      — forkd metadata (volumes, etc.)
//!   rootfs.ext4        — block device for child overlays
//! ```
//!
//! ## Pack format v2 (chain-aware, v0.5+)
//!
//! Emitted when the head snapshot has `parent_tag.is_some()` — i.e.
//! it's the tip of a v0.5 diff-snapshot chain. The bundle carries
//! every ancestor so the receiver can restore the whole chain in one
//! step without a separate `pull` for each link.
//!
//! ```text
//! tar.zst archive:
//!   manifest.toml      — chain[] lists every link root → head, with
//!                        per-link sha256s and parent edges
//!   <tag-0>/snapshot.json      ┐
//!   <tag-0>/vmstate            │ root base
//!   <tag-0>/memory.bin         │ (full memory)
//!   <tag-0>/rootfs.ext4        ┘
//!   <tag-1>/snapshot.json      ┐
//!   <tag-1>/vmstate            │ first diff link
//!   <tag-1>/memory.bin         │ (delta over <tag-0>)
//!   <tag-2>/...                  next link, etc.
//! ```
//!
//! Backward compat: bases (depth-0 chains) keep emitting v1 packs so
//! older `forkd unpack` clients keep working. Only chains of depth ≥ 1
//! force v2.
//!
//! The manifest's `forkd_pack_version` lets us evolve the format
//! without breaking older clients — `unpack` rejects any pack whose
//! version is newer than this binary supports.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;
use std::time::Instant;

/// Current pack format version emitted by `pack` for chained
/// snapshots. v1 packs are still emitted for depth-0 (base) snapshots
/// so older clients can keep unpacking the common case.
pub const PACK_FORMAT_VERSION: u32 = 2;

/// Maximum pack format version this binary's `unpack` understands.
/// Bump in lockstep with `PACK_FORMAT_VERSION` whenever a new layout
/// is added. Older packs unpack via their version-specific path.
pub const MAX_SUPPORTED_PACK_VERSION: u32 = 2;

/// `manifest.toml` shipped at the root of every pack.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    /// Format version; rejected on mismatch (too new for this binary).
    pub forkd_pack_version: u32,
    /// Head tag — for v1 packs, the only tag in the bundle. For v2
    /// packs (chains), the *head* of the chain (= `chain.last().tag`).
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
    /// v1 legacy field. For v1 packs: the head snapshot's parent tag
    /// (always None pre-v0.5 since chain support wasn't shipped).
    /// For v2 packs: equals `chain.last().parent_tag` for back-compat
    /// when v1 readers peek at this field; the canonical chain edges
    /// live under `chain[].parent_tag`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_tag: Option<String>,
    /// v1 layout: per-file metadata for the single snapshot the pack
    /// contains. Paths are tar-root-relative.
    ///
    /// v2 layout: empty (use `chain[].files` instead). v1 readers that
    /// haven't been taught v2 yet would loop over this expecting files
    /// at the tar root and find none — they'd then fail integrity
    /// check or the surrounding `version > supported` guard, never
    /// silently extracting nothing.
    #[serde(default)]
    pub files: Vec<FileEntry>,
    /// v2 chain layout. Empty for v1 packs. Index 0 is the chain root
    /// (base, `parent_tag = None`); the last entry is the head and
    /// matches `Manifest::tag`. Each link's `files` paths are
    /// relative to `<tag>/` inside the tar.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chain: Vec<ChainLinkMeta>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

/// One link in a v2 chain pack. Files live under `<tag>/` in the tar
/// so a 3-deep chain bundles 3 distinct snapshot dirs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainLinkMeta {
    /// Tag of this link. Validated against `is_safe_tag`-equivalent
    /// rules at unpack time so a malicious pack can't write outside
    /// `snapshot_root`.
    pub tag: String,
    /// Direct parent in the chain. `None` for the root base.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_tag: Option<String>,
    /// sha256 of the parent's `memory.bin` at chain-build time.
    /// `None` for the root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_content_hash: Option<String>,
    /// Per-file metadata. Paths are relative to the tar entry
    /// `<tag>/<path>`. Same fields and meaning as v1's [`Manifest::files`].
    pub files: Vec<FileEntry>,
}

/// Files that make up a snapshot directory. Order matters: largest last
/// so progress reporting reads roughly increasing.
const SNAPSHOT_FILES: &[&str] = &["snapshot.json", "vmstate", "rootfs.ext4", "memory.bin"];

/// v1 pack format constant — kept distinct from `PACK_FORMAT_VERSION`
/// (which tracks the *current* writer's preferred version) so callers
/// can explicitly request the legacy layout for back-compat.
const PACK_FORMAT_VERSION_V1: u32 = 1;

/// Pack a local snapshot directory into a single `.forkd-snapshot.tar.zst`.
///
/// Emits v1 format (single-snapshot layout) when the snapshot has no
/// `parent_tag` — preserves the wire format for the common base-image
/// case so older `forkd unpack` clients keep working.
///
/// For chained snapshots (`parent_tag.is_some()`), pivots to
/// [`pack_chain`] and emits a v2 chain bundle. Callers don't need to
/// know which path was taken — the returned `Manifest` is
/// self-describing.
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
    // v0.5: peek at snapshot.json to decide v1 vs v2. A snapshot dir
    // with `parent_tag` set isn't self-restorable on its own — its
    // `memory.bin` is a delta — so packing it alone would produce a
    // broken pack. Walk the chain instead.
    let head_meta = read_local_snapshot_meta(snap_dir);
    if head_meta
        .as_ref()
        .and_then(|m| m.parent_tag.as_ref())
        .is_some()
    {
        let snap_root = snap_dir
            .parent()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "snap_dir {} has no parent — cannot resolve chain ancestors",
                    snap_dir.display()
                )
            })?
            .to_path_buf();
        return pack_chain(tag, description, base_image, &snap_root, snap_dir, out_path);
    }

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
        forkd_pack_version: PACK_FORMAT_VERSION_V1,
        tag: tag.to_string(),
        description,
        base_image,
        created_at: chrono_like_now(),
        forkd_version: env!("CARGO_PKG_VERSION").to_string(),
        parent_tag: None,
        files: files.clone(),
        chain: Vec::new(),
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

/// Pack a chain of snapshots (root → head) into one v2 bundle. The
/// caller has already determined that `snap_dir` is the chain head
/// (parent_tag.is_some()). Walks ancestors by reading each link's
/// `snapshot.json` from disk under `snap_root` — no daemon needed.
///
/// The resulting tar contains each link's files under `<tag>/` so the
/// receiver materializes one snapshot dir per link.
pub fn pack_chain(
    head_tag: &str,
    description: Option<String>,
    base_image: Option<String>,
    snap_root: &Path,
    head_dir: &Path,
    out_path: &Path,
) -> Result<Manifest> {
    // Walk parent edges back to a base. Each link is identified by its
    // *local* on-disk tag (= the snapshot dir name), not the head's
    // owner-qualified tag — we want the bundle's tar entries keyed by
    // the same names the receiver will register the snapshots under.
    let mut chain_back: Vec<(String, std::path::PathBuf, forkd_vmm::Snapshot)> = Vec::new();
    let head_local = local_tag_of(head_dir, head_tag)?;
    let head_meta = read_local_snapshot_meta(head_dir).ok_or_else(|| {
        anyhow::anyhow!(
            "head {} has no snapshot.json — cannot pack chain",
            head_dir.display()
        )
    })?;
    chain_back.push((head_local, head_dir.to_path_buf(), head_meta));
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    seen.insert(chain_back[0].0.clone());
    loop {
        let (_, _, ref tip_meta) = chain_back[chain_back.len() - 1];
        let Some(parent_tag) = tip_meta.parent_tag.clone() else {
            break;
        };
        if !seen.insert(parent_tag.clone()) {
            bail!("chain for `{head_tag}` reaches `{parent_tag}` twice — cycle");
        }
        let parent_dir = snap_root.join(&parent_tag);
        if !parent_dir.join("vmstate").exists() {
            bail!(
                "chain for `{head_tag}` references parent `{parent_tag}` at {} \
                 which doesn't exist locally. Pack failed — pull or rebuild \
                 the parent first.",
                parent_dir.display()
            );
        }
        let parent_meta = read_local_snapshot_meta(&parent_dir).ok_or_else(|| {
            anyhow::anyhow!(
                "parent `{parent_tag}` has no snapshot.json at {}",
                parent_dir.display()
            )
        })?;
        chain_back.push((parent_tag, parent_dir, parent_meta));
    }
    // Reverse to root → head ordering matching the manifest invariant.
    chain_back.reverse();

    // Hash + size every file under each link.
    let mut chain_meta: Vec<ChainLinkMeta> = Vec::with_capacity(chain_back.len());
    for (link_tag, link_dir, link_snap) in &chain_back {
        let mut files: Vec<FileEntry> = Vec::new();
        for name in SNAPSHOT_FILES {
            let p = link_dir.join(name);
            if !p.exists() {
                continue;
            }
            let meta = std::fs::metadata(&p).with_context(|| format!("stat {}", p.display()))?;
            files.push(FileEntry {
                path: (*name).to_string(),
                size: meta.len(),
                sha256: sha256_file(&p)?,
            });
        }
        if !files.iter().any(|f| f.path == "memory.bin") {
            bail!(
                "chain link `{link_tag}` at {} has no memory.bin — pack would \
                 produce a broken bundle",
                link_dir.display()
            );
        }
        chain_meta.push(ChainLinkMeta {
            tag: link_tag.clone(),
            parent_tag: link_snap.parent_tag.clone(),
            parent_content_hash: link_snap.parent_content_hash.clone(),
            files,
        });
    }

    let manifest = Manifest {
        forkd_pack_version: PACK_FORMAT_VERSION,
        tag: head_tag.to_string(),
        description,
        base_image,
        created_at: chrono_like_now(),
        forkd_version: env!("CARGO_PKG_VERSION").to_string(),
        // Carry forward the head's parent_tag at the top level so v1
        // readers that didn't grow chain support get something
        // informative when they peek at this field before rejecting
        // the version.
        parent_tag: chain_meta.last().and_then(|m| m.parent_tag.clone()),
        files: Vec::new(),
        chain: chain_meta.clone(),
    };

    let manifest_toml = toml::to_string_pretty(&manifest).context("serialize manifest")?;

    let out_file =
        File::create(out_path).with_context(|| format!("create {}", out_path.display()))?;
    let zstd_writer = zstd::Encoder::new(out_file, 0)
        .context("init zstd encoder")?
        .auto_finish();
    let mut tar = tar::Builder::new(zstd_writer);

    let manifest_bytes = manifest_toml.as_bytes();
    let mut header = tar::Header::new_gnu();
    header.set_size(manifest_bytes.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(unix_secs_now());
    header.set_cksum();
    tar.append_data(&mut header, "manifest.toml", manifest_bytes)
        .context("append manifest.toml")?;

    for ((link_tag, link_dir, _), link_meta) in chain_back.iter().zip(chain_meta.iter()) {
        for entry in &link_meta.files {
            let path = link_dir.join(&entry.path);
            let mut f = File::open(&path).with_context(|| format!("open {}", path.display()))?;
            let tar_path = format!("{}/{}", link_tag, entry.path);
            tar.append_file(&tar_path, &mut f)
                .with_context(|| format!("append {tar_path} to tar"))?;
        }
    }

    tar.finish().context("tar finish")?;
    Ok(manifest)
}

/// Read a snapshot.json from a local snap_dir into a [`forkd_vmm::Snapshot`].
/// Returns `None` when the file is missing or unparseable — for v0.4-and-
/// earlier base snapshots that pre-date snapshot.json (the "bases without
/// chain edges" case) callers should treat None as `parent_tag: None`.
fn read_local_snapshot_meta(snap_dir: &Path) -> Option<forkd_vmm::Snapshot> {
    std::fs::read(snap_dir.join("snapshot.json"))
        .ok()
        .and_then(|raw| serde_json::from_slice(&raw).ok())
}

/// The "local tag" of a snapshot dir is its directory name. For the
/// chain head pack() takes a caller-supplied owner-qualified tag (e.g.
/// `deeplethe/python-numpy+pandas`) — but the receiver registers it
/// under whatever the bundle says it is, so we fall back to the
/// supplied tag when its safe-tag shape matches the local dir name,
/// or to the dir name otherwise.
fn local_tag_of(snap_dir: &Path, fallback: &str) -> Result<String> {
    let dir_name = snap_dir
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| {
            anyhow::anyhow!("snap_dir {} has no usable file name", snap_dir.display())
        })?;
    // Prefer the caller-supplied tag if it matches the dir name
    // (= same identity, just owner-qualified). Otherwise use the dir
    // name (the receiver doesn't need to know the publisher's owner).
    if dir_name == fallback {
        Ok(fallback.to_string())
    } else {
        Ok(dir_name.to_string())
    }
}

/// Unpack a `.forkd-snapshot.tar.zst` into `dest_dir`. Verifies the
/// manifest's pack-format version and each file's sha256.
///
/// For v1 packs: `dest_dir` ends up containing snapshot.json /
/// vmstate / memory.bin / rootfs.ext4 directly (legacy layout).
///
/// For v2 packs: `dest_dir` ends up containing one subdirectory per
/// chain link (`<dest_dir>/<tag>/<file>`). The caller (`unpack_into`)
/// then renames each subdirectory into its final snapshot_dir.
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
            if m.forkd_pack_version > MAX_SUPPORTED_PACK_VERSION {
                bail!(
                    "pack format version {} is newer than this forkd supports (max {}). \
                     Upgrade forkd or repack with an older version.",
                    m.forkd_pack_version,
                    MAX_SUPPORTED_PACK_VERSION,
                );
            }
            // Reject v2 packs whose chain[] references unsafe tag
            // names. We check up-front (before extracting any file
            // bodies) so a malicious bundle with `tag = "../../etc"`
            // never lands on disk.
            for link in &m.chain {
                if !is_safe_pack_tag(&link.tag) {
                    bail!(
                        "v2 pack chain link declares unsafe tag {:?} — refusing to extract",
                        link.tag
                    );
                }
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
    // recorded sha256. v1 packs declare files at the top level; v2 packs
    // declare them under `<link.tag>/<path>`.
    if manifest.chain.is_empty() {
        // v1 layout.
        for entry in &manifest.files {
            verify_one(&dest_dir.join(&entry.path), entry)?;
        }
        rewrite_snapshot_paths(dest_dir)?;
    } else {
        // v2 chain layout.
        for link in &manifest.chain {
            let link_dir = dest_dir.join(&link.tag);
            for entry in &link.files {
                verify_one(&link_dir.join(&entry.path), entry)?;
            }
            rewrite_snapshot_paths(&link_dir)?;
        }
    }

    Ok(manifest)
}

/// Rewrite `snapshot.json`'s absolute `vmstate` / `memory` paths to point
/// into the directory the snapshot was just extracted into.
///
/// Packs carry the *packing host's* absolute paths (e.g.
/// `/home/<packer>/.local/share/forkd/snapshots/<tag>/vmstate`), which are
/// meaningless on the pulling machine — without this fixup the first
/// restore dies with Firecracker's "Failed to open snapshot file: No such
/// file or directory". Runs after sha256 verification so the integrity
/// check still sees the bytes the packer signed.
///
/// Operates on `serde_json::Value` rather than the typed
/// `forkd_vmm::Snapshot` so fields this forkd version doesn't know about
/// survive the round-trip. Volume host paths are left untouched — they
/// reference paths outside the snapshot dir that we can't relocate.
///
/// Idempotent, and `pub(crate)` because callers that extract into a
/// staging dir and `rename(2)` to the final location (`unpack_into`)
/// must run it AGAIN post-move — the in-`unpack` pass points paths at
/// the staging dir, which goes stale the moment the rename happens.
pub(crate) fn rewrite_snapshot_paths(dir: &Path) -> Result<()> {
    let sj = dir.join("snapshot.json");
    if !sj.exists() {
        return Ok(());
    }
    let raw = std::fs::read_to_string(&sj)
        .with_context(|| format!("read {} for path fixup", sj.display()))?;
    let mut v: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("parse {} for path fixup", sj.display()))?;
    if let Some(obj) = v.as_object_mut() {
        for key in ["vmstate", "memory"] {
            let Some(s) = obj.get(key).and_then(|x| x.as_str()) else {
                continue;
            };
            let Some(name) = Path::new(s).file_name() else {
                continue;
            };
            let local = dir.join(name);
            if local.exists() && local.to_str() != Some(s) {
                obj.insert(
                    key.to_string(),
                    serde_json::Value::String(local.to_string_lossy().into_owned()),
                );
            }
        }
    }
    let out = serde_json::to_string_pretty(&v).context("re-serialize snapshot.json")?;
    std::fs::write(&sj, out).with_context(|| format!("write fixed-up {}", sj.display()))?;
    Ok(())
}

/// Verify a single extracted file against its [`FileEntry`] manifest
/// declaration. Shared between v1 and v2 unpack paths.
fn verify_one(path: &Path, entry: &FileEntry) -> Result<()> {
    let actual = sha256_file(path).with_context(|| {
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
    Ok(())
}

/// Tag-name safety check used at pack-unpack boundaries. Mirrors the
/// daemon's `is_safe_tag` rule so a v2 pack can't smuggle a path-
/// traversal tag through `chain[].tag`.
fn is_safe_pack_tag(tag: &str) -> bool {
    !tag.is_empty()
        && tag.len() <= 64
        && tag
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
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

    /// Regression: packs bake the packing host's absolute paths into
    /// snapshot.json. `unpack` must rewrite `vmstate` / `memory` to the
    /// extraction dir or the first restore on any other machine fails
    /// with "Failed to open snapshot file". Unknown fields and volume
    /// paths must survive untouched.
    #[test]
    fn rewrite_snapshot_paths_relocates_vmstate_and_memory() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join("vmstate"), b"x").unwrap();
        std::fs::write(dir.join("memory.bin"), b"x").unwrap();
        std::fs::write(
            dir.join("snapshot.json"),
            r#"{
  "vmstate": "/home/packer/.local/share/forkd/snapshots/t/vmstate",
  "memory": "/home/packer/.local/share/forkd/snapshots/t/memory.bin",
  "volumes": [{"host_path": "/data/vol.ext4"}],
  "some_future_field": 42
}"#,
        )
        .unwrap();

        rewrite_snapshot_paths(dir).unwrap();

        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("snapshot.json")).unwrap())
                .unwrap();
        assert_eq!(
            v["vmstate"].as_str().unwrap(),
            dir.join("vmstate").to_str().unwrap()
        );
        assert_eq!(
            v["memory"].as_str().unwrap(),
            dir.join("memory.bin").to_str().unwrap()
        );
        // Untouched: volumes + unknown fields.
        assert_eq!(
            v["volumes"][0]["host_path"].as_str().unwrap(),
            "/data/vol.ext4"
        );
        assert_eq!(v["some_future_field"].as_i64().unwrap(), 42);
    }

    /// When the referenced file isn't in the extraction dir (or there is
    /// no snapshot.json at all), rewrite must be a no-op rather than
    /// pointing paths at nonexistent files.
    #[test]
    fn rewrite_snapshot_paths_noop_when_files_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        // No snapshot.json: must not error.
        rewrite_snapshot_paths(dir).unwrap();

        // snapshot.json present but vmstate/memory files absent: keep
        // the original paths.
        std::fs::write(
            dir.join("snapshot.json"),
            r#"{"vmstate": "/elsewhere/vmstate", "memory": "/elsewhere/memory.bin"}"#,
        )
        .unwrap();
        rewrite_snapshot_paths(dir).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("snapshot.json")).unwrap())
                .unwrap();
        assert_eq!(v["vmstate"].as_str().unwrap(), "/elsewhere/vmstate");
        assert_eq!(v["memory"].as_str().unwrap(), "/elsewhere/memory.bin");
    }

    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn pack_format_versions_are_in_lockstep() {
        // v0.5 Phase 3: writer emits v2 for chained snapshots; reader
        // accepts both v1 and v2. The "max supported" constant must
        // be ≥ what the writer emits, else our own packs wouldn't
        // unpack — exact equality is the invariant we want to defend.
        // Clippy flags `PACK_FORMAT_VERSION_V1 < PACK_FORMAT_VERSION`
        // as "constant comparison", but that's exactly the point: this
        // test fails *at compile time* if a future bump breaks the
        // V1 < CURRENT ordering invariant.
        assert_eq!(PACK_FORMAT_VERSION, 2);
        assert_eq!(MAX_SUPPORTED_PACK_VERSION, 2);
        assert!(PACK_FORMAT_VERSION_V1 < PACK_FORMAT_VERSION);
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
            chain: Vec::new(),
        };
        let s = toml::to_string_pretty(&m).unwrap();
        let m2: Manifest = toml::from_str(&s).unwrap();
        assert_eq!(m.tag, m2.tag);
        assert_eq!(m.files.len(), m2.files.len());
        assert_eq!(m.files[0].sha256, m2.files[0].sha256);
        assert!(
            m2.chain.is_empty(),
            "v1 manifest must round-trip with empty chain"
        );
    }

    #[test]
    fn manifest_v2_chain_roundtrip() {
        // v2 manifest with a 2-link chain. Ensures the chain[] field
        // serializes/deserializes cleanly and that v1's `files` field
        // can stay empty without tripping defaults.
        let m = Manifest {
            forkd_pack_version: 2,
            tag: "deeplethe/python-numpy-pandas".into(),
            description: None,
            base_image: None,
            created_at: "2026-06-04T15:00:00Z".into(),
            forkd_version: "0.3.4".into(),
            parent_tag: Some("python-numpy".into()),
            files: Vec::new(),
            chain: vec![
                ChainLinkMeta {
                    tag: "python-numpy".into(),
                    parent_tag: None,
                    parent_content_hash: None,
                    files: vec![FileEntry {
                        path: "memory.bin".into(),
                        size: 4096,
                        sha256: "base-hash".into(),
                    }],
                },
                ChainLinkMeta {
                    tag: "python-numpy-pandas".into(),
                    parent_tag: Some("python-numpy".into()),
                    parent_content_hash: Some("base-hash".into()),
                    files: vec![FileEntry {
                        path: "memory.bin".into(),
                        size: 4096,
                        sha256: "diff-hash".into(),
                    }],
                },
            ],
        };
        let s = toml::to_string_pretty(&m).unwrap();
        let m2: Manifest = toml::from_str(&s).unwrap();
        assert_eq!(m2.chain.len(), 2);
        assert_eq!(m2.chain[0].parent_tag, None);
        assert_eq!(m2.chain[1].parent_tag, Some("python-numpy".into()));
        assert_eq!(m2.chain[1].parent_content_hash, Some("base-hash".into()));
        assert!(m2.files.is_empty());
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

    /// Helper: build a 2-link chain on disk under `snap_root`.
    /// Returns (head_tag, head_dir).
    fn build_chain_fixture(snap_root: &Path) -> (String, std::path::PathBuf) {
        let base_dir = snap_root.join("py-base");
        std::fs::create_dir(&base_dir).unwrap();
        std::fs::write(base_dir.join("vmstate"), b"base-vmstate-bytes").unwrap();
        let base_memory = vec![0xAAu8; 4096];
        std::fs::write(base_dir.join("memory.bin"), &base_memory).unwrap();
        let base_snap = forkd_vmm::Snapshot {
            vmstate: base_dir.join("vmstate"),
            memory: base_dir.join("memory.bin"),
            volumes: Vec::new(),
            parent_tag: None,
            parent_content_hash: None,
        };
        std::fs::write(
            base_dir.join("snapshot.json"),
            serde_json::to_vec_pretty(&base_snap).unwrap(),
        )
        .unwrap();
        let base_hash = sha256_file(&base_dir.join("memory.bin")).unwrap();

        let head_dir = snap_root.join("py-numpy");
        std::fs::create_dir(&head_dir).unwrap();
        std::fs::write(head_dir.join("vmstate"), b"head-vmstate-bytes").unwrap();
        std::fs::write(head_dir.join("memory.bin"), vec![0xBBu8; 4096]).unwrap();
        let head_snap = forkd_vmm::Snapshot {
            vmstate: head_dir.join("vmstate"),
            memory: head_dir.join("memory.bin"),
            volumes: Vec::new(),
            parent_tag: Some("py-base".to_string()),
            parent_content_hash: Some(base_hash),
        };
        std::fs::write(
            head_dir.join("snapshot.json"),
            serde_json::to_vec_pretty(&head_snap).unwrap(),
        )
        .unwrap();
        ("py-numpy".to_string(), head_dir)
    }

    #[test]
    fn pack_emits_v1_for_base_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let snap_root = tmp.path().join("snap_root");
        std::fs::create_dir(&snap_root).unwrap();
        let base_dir = snap_root.join("py-base");
        std::fs::create_dir(&base_dir).unwrap();
        std::fs::write(base_dir.join("vmstate"), b"vmstate").unwrap();
        std::fs::write(base_dir.join("memory.bin"), vec![0u8; 4096]).unwrap();
        let base_snap = forkd_vmm::Snapshot {
            vmstate: base_dir.join("vmstate"),
            memory: base_dir.join("memory.bin"),
            volumes: Vec::new(),
            parent_tag: None,
            parent_content_hash: None,
        };
        std::fs::write(
            base_dir.join("snapshot.json"),
            serde_json::to_vec_pretty(&base_snap).unwrap(),
        )
        .unwrap();

        let pack_out = tmp.path().join("base.tar.zst");
        let manifest = pack("py-base", None, None, &base_dir, &pack_out).unwrap();
        assert_eq!(
            manifest.forkd_pack_version, PACK_FORMAT_VERSION_V1,
            "base snapshots must keep emitting v1 so older clients can still unpack them"
        );
        assert!(manifest.chain.is_empty());
        assert!(!manifest.files.is_empty());
    }

    #[test]
    fn pack_emits_v2_for_chained_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let snap_root = tmp.path().join("snap_root");
        std::fs::create_dir(&snap_root).unwrap();
        let (head_tag, head_dir) = build_chain_fixture(&snap_root);

        let pack_out = tmp.path().join("chain.tar.zst");
        let manifest = pack(&head_tag, None, None, &head_dir, &pack_out).unwrap();
        assert_eq!(manifest.forkd_pack_version, 2);
        assert_eq!(manifest.chain.len(), 2);
        assert_eq!(manifest.chain[0].tag, "py-base");
        assert_eq!(manifest.chain[0].parent_tag, None);
        assert_eq!(manifest.chain[1].tag, "py-numpy");
        assert_eq!(manifest.chain[1].parent_tag, Some("py-base".to_string()));
        // Manifest's legacy `parent_tag` field is set to the head's
        // parent so v1 readers peeking at it get something meaningful
        // before they reject the version.
        assert_eq!(manifest.parent_tag, Some("py-base".to_string()));
        assert!(
            manifest.files.is_empty(),
            "v2 manifests must keep top-level files empty"
        );
    }

    #[test]
    fn pack_v2_then_unpack_recreates_all_chain_links() {
        // The full v0.5 Phase 3 round-trip: build chain → pack → unpack
        // → assert both links materialized with their original bytes.
        let tmp = tempfile::tempdir().unwrap();
        let snap_root = tmp.path().join("snap_root");
        std::fs::create_dir(&snap_root).unwrap();
        let (head_tag, head_dir) = build_chain_fixture(&snap_root);

        let pack_out = tmp.path().join("chain.tar.zst");
        let _ = pack(&head_tag, None, None, &head_dir, &pack_out).unwrap();

        let unpack_root = tmp.path().join("dst");
        std::fs::create_dir(&unpack_root).unwrap();
        let unpacked = unpack(&pack_out, &unpack_root).unwrap();
        assert_eq!(unpacked.chain.len(), 2);

        // Both link directories exist under unpack_root and contain
        // bytes-identical content to the source fixture.
        let base_unpacked = unpack_root.join("py-base");
        let head_unpacked = unpack_root.join("py-numpy");
        assert!(base_unpacked.join("memory.bin").exists());
        assert!(head_unpacked.join("memory.bin").exists());
        assert_eq!(
            std::fs::read(base_unpacked.join("memory.bin")).unwrap(),
            vec![0xAAu8; 4096]
        );
        assert_eq!(
            std::fs::read(head_unpacked.join("memory.bin")).unwrap(),
            vec![0xBBu8; 4096]
        );
        // snapshot.json files round-trip with chain edges intact.
        let head_snap_raw = std::fs::read(head_unpacked.join("snapshot.json")).unwrap();
        let head_snap: forkd_vmm::Snapshot = serde_json::from_slice(&head_snap_raw).unwrap();
        assert_eq!(head_snap.parent_tag, Some("py-base".to_string()));
    }

    #[test]
    fn unpack_rejects_v2_pack_with_unsafe_chain_tag() {
        // Defense-in-depth: craft a v2 manifest with `chain[].tag =
        // "../etc"` and confirm unpack refuses before extracting files.
        let tmp = tempfile::tempdir().unwrap();
        let pack_out = tmp.path().join("evil.tar.zst");

        let evil = Manifest {
            forkd_pack_version: 2,
            tag: "innocent".into(),
            description: None,
            base_image: None,
            created_at: "now".into(),
            forkd_version: "0".into(),
            parent_tag: None,
            files: Vec::new(),
            chain: vec![ChainLinkMeta {
                tag: "../etc".into(),
                parent_tag: None,
                parent_content_hash: None,
                files: vec![],
            }],
        };
        let manifest_toml = toml::to_string_pretty(&evil).unwrap();
        let out_file = File::create(&pack_out).unwrap();
        let zstd_writer = zstd::Encoder::new(out_file, 0).unwrap().auto_finish();
        let mut tar = tar::Builder::new(zstd_writer);
        let manifest_bytes = manifest_toml.as_bytes();
        let mut header = tar::Header::new_gnu();
        header.set_size(manifest_bytes.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(0);
        header.set_cksum();
        tar.append_data(&mut header, "manifest.toml", manifest_bytes)
            .unwrap();
        tar.finish().unwrap();
        drop(tar);

        let dst = tmp.path().join("dst");
        let err = unpack(&pack_out, &dst).expect_err("must reject unsafe chain tag");
        let msg = format!("{err}");
        assert!(
            msg.contains("unsafe tag"),
            "error must name the unsafe-tag refusal: {msg}"
        );
    }
}
