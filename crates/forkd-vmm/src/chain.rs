//! Diff-snapshot chain resolution and assembly (v0.5 / M2.1).
//!
//! Phase 1 of [`DESIGN-v0.5-diff-snapshot-chains.md`](../../../DESIGN-v0.5-diff-snapshot-chains.md).
//! Given a snapshot tag that may be the head of a chain, walk
//! `parent_tag` to the base and assemble a single `memory.bin` on
//! disk by `cp(base) + apply_diff(each link)`.
//!
//! Design choices baked in here:
//!
//! - **Chain shape is delta** ([Storage shape](../../../DESIGN-v0.5-diff-snapshot-chains.md#storage-shape)).
//!   Each chained snapshot's `memory` field points at a sparse FC
//!   Diff file vs. the parent.
//! - **Reflink preferred** for the base copy. On reflink-capable
//!   filesystems (btrfs, xfs, ext4 with the `reflink` feature flag)
//!   this collapses the cp to a metadata-only operation, hitting
//!   ROADMAP done-criterion 2. Falls back to plain `read+write` on
//!   non-reflink FS with a one-time warning.
//! - **Content-hash pinning** on parents — the first-pass design's
//!   "name-only pinning" foot-gun is fixed in this revision.
//!   Restore verifies the parent's current `memory.bin` against
//!   `parent_content_hash`; mismatch errors out clearly.
//! - **No file-management ownership.** The caller passes in a scratch
//!   path for the assembled output and owns its lifetime. This lets
//!   `forkd-controller` clean up per-sandbox scratch on sandbox
//!   teardown without `forkd-vmm` having to know about sandbox state.
//!
//! See the `tests` submodule for the seven Phase 1 unit cases
//! enumerated in the design doc.

use anyhow::{bail, Context, Result};
use std::path::Path;

use crate::Snapshot;

/// Maximum chain depth before [`resolve_chain`] errors out. Catches
/// pathological inputs (deeply nested chains the user didn't mean
/// to create) and acts as the cycle-detection ceiling.
///
/// 32 is generous: ROADMAP M2.1 contemplates 1-3 typical levels,
/// and we [warn at 5](../../../DESIGN-v0.5-diff-snapshot-chains.md#1-maximum-chain-depth--what-to-enforce).
/// The hard cap exists to bound restore-time work, not to enforce
/// a policy. Callers that want a tighter policy should inspect the
/// returned `Vec`'s length and refuse.
pub const MAX_CHAIN_DEPTH: usize = 32;

/// Walk parents from the chain head down to the base.
///
/// `lookup` is `tag -> Snapshot` for resolving each parent tag to its
/// metadata. Caller injects this so we don't have to plumb the
/// controller's `Registry` into `forkd-vmm`; for one-shot CLI flows a
/// closure over `<snapshot_root>/<tag>/snapshot.json` works.
///
/// Returns the chain in **base-first order**: `[base, +pandas, +sklearn]`
/// — same order `assemble_chain_memory` consumes.
///
/// Errors with actionable messages on:
/// - Missing parent (parent_tag references a tag the lookup can't find)
/// - Cycle (any tag repeated in the chain)
/// - Depth exceeded ([`MAX_CHAIN_DEPTH`])
pub fn resolve_chain<F>(head_tag: &str, lookup: F) -> Result<Vec<(String, Snapshot)>>
where
    F: Fn(&str) -> Result<Snapshot>,
{
    // Build head-to-base, then reverse. We need both the tag string
    // and the resolved Snapshot to detect cycles and to look up the
    // next parent, so the return type carries both.
    let mut chain: Vec<(String, Snapshot)> = Vec::new();
    let mut current_tag = head_tag.to_string();

    for _ in 0..=MAX_CHAIN_DEPTH {
        // Cycle check: if we've already seen this tag in the chain
        // we're in a loop. Use linear scan because depths are tiny.
        if chain.iter().any(|(t, _)| t == &current_tag) {
            bail!(
                "snapshot chain has a cycle: `{}` is reachable from itself via parent_tag",
                current_tag
            );
        }
        let snap = lookup(&current_tag).with_context(|| {
            format!(
                "resolve chain link `{}` (parent of `{}`)",
                current_tag,
                chain.last().map(|(t, _)| t.as_str()).unwrap_or(head_tag),
            )
        })?;
        let next_parent = snap.parent_tag.clone();
        chain.push((current_tag.clone(), snap));
        match next_parent {
            Some(p) => current_tag = p,
            // We hit the base.
            None => {
                chain.reverse();
                return Ok(chain);
            }
        }
    }
    bail!(
        "snapshot chain for `{}` exceeds MAX_CHAIN_DEPTH={} — use `forkd snapshot compact` to flatten",
        head_tag,
        MAX_CHAIN_DEPTH
    );
}

/// SHA-256 a file's contents, hex-encoded. Used both at build time (to
/// record `parent_content_hash`) and at restore time (to verify it).
///
/// Streams in 1 MiB chunks so a 4 GiB memory.bin doesn't allocate
/// 4 GiB of intermediate buffer.
pub fn sha256_file(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;

    let mut f = std::fs::File::open(path)
        .with_context(|| format!("sha256_file: open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf).context("sha256_file: read")?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex_encode(&hasher.finalize()))
}

/// Minimal hex encoder for the sha256 output. Avoids pulling in the
/// `hex` crate for one use site.
fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

/// Verify every chained link's `parent_content_hash` matches its
/// parent's current `memory.bin`. The first-pass design deferred
/// hash-pinning to v0.6, but a silent foot-gun isn't acceptable for
/// the v0.5 GA; this is the fix.
///
/// `chain` must be in base-first order (as returned by
/// [`resolve_chain`]). Skips the base (`chain[0]`) since bases have
/// no parent to verify.
///
/// Returns the actionable error on mismatch. Cheap when the chain is
/// short and `memory.bin`s fit in page cache; for long chains over
/// cold storage this is the dominant restore-time cost.
pub fn verify_parent_hashes(chain: &[(String, Snapshot)]) -> Result<()> {
    for window in chain.windows(2) {
        let (parent_tag, parent_snap) = &window[0];
        let (child_tag, child_snap) = &window[1];
        let recorded = match &child_snap.parent_content_hash {
            Some(h) => h,
            None => bail!(
                "chained snapshot `{}` has parent_tag=`{}` but no parent_content_hash; \
                 rebuild this snapshot — chains created before v0.5 GA aren't supported",
                child_tag,
                parent_tag
            ),
        };
        let actual = sha256_file(&parent_snap.memory).with_context(|| {
            format!(
                "hash parent memory for chain verification: `{}` at {}",
                parent_tag,
                parent_snap.memory.display()
            )
        })?;
        if actual != *recorded {
            bail!(
                "chain `{}` references parent `{}` with content `{}…`, but parent now has \
                 content `{}…`; rebuild with `forkd snapshot diff --from {}`",
                child_tag,
                parent_tag,
                &recorded[..12.min(recorded.len())],
                &actual[..12.min(actual.len())],
                parent_tag,
            );
        }
    }
    Ok(())
}

/// Assemble a chain's memory image into `out_path`.
///
/// Steps:
/// 1. Copy the base's `memory.bin` to `out_path` (reflink-preferred,
///    falls back to plain copy on non-reflink FS).
/// 2. For each subsequent link in `chain`, [`crate::apply_diff`] its
///    `memory` (a sparse diff) onto `out_path`.
/// 3. Return the total bytes copied (= base size + sum of dirty
///    pages across diffs).
///
/// `out_path` must not exist; the caller picks the scratch location
/// and owns cleanup. For a single-link chain (just the base) this
/// degenerates to a single `cp`.
///
/// The base copy uses `cp --reflink=auto`-equivalent semantics via
/// the libc `ioctl(FICLONE)` Linux syscall when the destination FS
/// supports it (returns metadata-only success); falls back to a
/// stream copy with a one-time warn-level log otherwise.
pub fn assemble_chain_memory(chain: &[(String, Snapshot)], out_path: &Path) -> Result<u64> {
    if chain.is_empty() {
        bail!("assemble_chain_memory: empty chain");
    }
    if out_path.exists() {
        bail!(
            "assemble_chain_memory: out_path {} already exists — caller owns scratch lifetime",
            out_path.display()
        );
    }

    let (base_tag, base_snap) = &chain[0];
    let base_bytes = copy_base_memory(&base_snap.memory, out_path).with_context(|| {
        format!(
            "copy base memory for chain (base=`{}`, src={}, dst={})",
            base_tag,
            base_snap.memory.display(),
            out_path.display()
        )
    })?;
    let mut total = base_bytes;

    for (child_tag, child_snap) in &chain[1..] {
        let copied = crate::apply_diff(&child_snap.memory, out_path).with_context(|| {
            format!(
                "apply chain link `{}` ({}) onto {}",
                child_tag,
                child_snap.memory.display(),
                out_path.display()
            )
        })?;
        total += copied;
    }
    Ok(total)
}

/// Copy `src` to `dst`. Tries reflink (ioctl `FICLONE`) first; falls
/// back to a regular streamed copy on non-reflink FS, logging once.
///
/// Linux only on the reflink path; non-Linux builds always use the
/// stream copy.
#[cfg(target_os = "linux")]
fn copy_base_memory(src: &Path, dst: &Path) -> Result<u64> {
    use std::os::unix::io::AsRawFd;

    let src_f = std::fs::File::open(src).with_context(|| format!("open base {}", src.display()))?;
    // CREATE+EXCL because we already checked the caller's path
    // doesn't exist; the caller-owns-scratch contract means we
    // shouldn't accidentally clobber.
    let dst_f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(dst)
        .with_context(|| format!("create out {}", dst.display()))?;

    // FICLONE: clone the entire src into dst. Returns 0 on success;
    // EINVAL/EXDEV/EOPNOTSUPP signal "this FS doesn't support
    // reflink for this pair," in which case we fall back. ENOTSUP
    // sometimes appears too.
    // ioctl number: _IO(0x94, 9). 0x94 is the BTRFS_IOCTL_MAGIC also
    // used by ficlone (overlayfs, btrfs, xfs, ext4-reflink).
    const FICLONE: libc::c_ulong = 0x4020_9409;
    // SAFETY: both fds are valid open file descriptors; FICLONE
    // takes the source fd as its argument.
    let rc = unsafe { libc::ioctl(dst_f.as_raw_fd(), FICLONE, src_f.as_raw_fd()) };
    if rc == 0 {
        // Reflink success. Size = src's size; no bytes copied to
        // disk yet (extents shared CoW). Return the logical size.
        let len = src_f
            .metadata()
            .with_context(|| format!("stat src {} post-reflink", src.display()))?
            .len();
        return Ok(len);
    }
    let errno = std::io::Error::last_os_error();
    let raw = errno.raw_os_error().unwrap_or(0);
    // EINVAL / EXDEV / EOPNOTSUPP / ENOTTY all mean "reflink not
    // available for this pair"; that's expected on ext4-without-
    // reflink and tmpfs. (ENOTSUP aliases to EOPNOTSUPP on Linux —
    // libc::ENOTSUP == libc::EOPNOTSUPP, so listing one covers both.)
    // Other errnos are real failures.
    if !matches!(
        raw,
        libc::EINVAL | libc::EXDEV | libc::EOPNOTSUPP | libc::ENOTTY
    ) {
        return Err(errno).context("FICLONE on base memory");
    }

    // Fall through to stream copy. Same fds; rewind both and copy.
    fallback_stream_copy(src_f, dst_f, src, dst)
}

#[cfg(not(target_os = "linux"))]
fn copy_base_memory(src: &Path, dst: &Path) -> Result<u64> {
    let src_f = std::fs::File::open(src).with_context(|| format!("open base {}", src.display()))?;
    let dst_f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dst)
        .with_context(|| format!("create out {}", dst.display()))?;
    fallback_stream_copy(src_f, dst_f, src, dst)
}

/// Last-resort full read+write of the base. Used when the host FS
/// doesn't support reflink for this `(src, dst)` pair.
fn fallback_stream_copy(
    mut src_f: std::fs::File,
    mut dst_f: std::fs::File,
    src: &Path,
    dst: &Path,
) -> Result<u64> {
    use std::io::{copy, Seek, SeekFrom};
    // We tried reflink and failed mid-call on Linux; on non-Linux
    // builds we skipped straight here. Rewind both fds either way.
    src_f
        .seek(SeekFrom::Start(0))
        .with_context(|| format!("rewind src {} for stream copy", src.display()))?;
    dst_f
        .seek(SeekFrom::Start(0))
        .with_context(|| format!("rewind dst {} for stream copy", dst.display()))?;
    let n = copy(&mut src_f, &mut dst_f)
        .with_context(|| format!("stream copy {} -> {}", src.display(), dst.display()))?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VolumeSpec;
    use std::collections::HashMap;
    use std::io::Write;
    use std::path::PathBuf;

    /// Build a base + diff pair on disk for the assemble tests.
    /// Base is 16 KiB of `base_byte`; diff has data in [4096..8192)
    /// of `delta_byte`, hole elsewhere. Returns the temp dir holding
    /// both files (caller drops to clean up).
    fn make_base_and_diff(label: &str, base_byte: u8, delta_byte: u8) -> PathBuf {
        let tmp = std::env::temp_dir().join(format!(
            "chain-test-{}-{}-{}",
            label,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let base_path = tmp.join("base.bin");
        let diff_path = tmp.join("diff.bin");

        let mut base_f = std::fs::File::create(&base_path).unwrap();
        base_f.write_all(&vec![base_byte; 16 * 1024]).unwrap();
        base_f.sync_all().unwrap();
        drop(base_f);

        // Diff: same logical size; data only in a 4 KiB window.
        let mut diff_f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&diff_path)
            .unwrap();
        diff_f.set_len(16 * 1024).unwrap();
        use std::io::{Seek, SeekFrom};
        diff_f.seek(SeekFrom::Start(4096)).unwrap();
        diff_f.write_all(&vec![delta_byte; 4096]).unwrap();
        diff_f.sync_all().unwrap();
        drop(diff_f);

        tmp
    }

    fn snap_at(memory: &Path, parent: Option<&str>, hash: Option<&str>) -> Snapshot {
        Snapshot {
            vmstate: memory.parent().unwrap().join("vmstate"),
            memory: memory.to_path_buf(),
            volumes: Vec::<VolumeSpec>::new(),
            parent_tag: parent.map(String::from),
            parent_content_hash: hash.map(String::from),
        }
    }

    // --- Phase 1 unit tests (1/7): resolve_chain on a base -----------

    #[test]
    fn resolve_chain_base_returns_singleton() {
        let dir = make_base_and_diff("base-only", 0xAA, 0x00);
        let base = snap_at(&dir.join("base.bin"), None, None);
        let mut tags = HashMap::new();
        tags.insert("base".to_string(), base.clone());

        let chain = resolve_chain("base", |t| tags.get(t).cloned().context("missing")).unwrap();
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].0, "base");
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- Phase 1 unit tests (2/7): resolve_chain on depth-3 ----------

    #[test]
    fn resolve_chain_depth_3_orders_base_first() {
        let dir = make_base_and_diff("depth-3", 0xAA, 0xBB);
        let base = snap_at(&dir.join("base.bin"), None, None);
        let pandas = snap_at(&dir.join("diff.bin"), Some("base"), Some("h-base"));
        let sklearn = snap_at(&dir.join("diff.bin"), Some("pandas"), Some("h-pandas"));
        let mut tags = HashMap::new();
        tags.insert("base".to_string(), base);
        tags.insert("pandas".to_string(), pandas);
        tags.insert("sklearn".to_string(), sklearn);

        let chain = resolve_chain("sklearn", |t| tags.get(t).cloned().context("missing")).unwrap();
        let tags_in_order: Vec<_> = chain.iter().map(|(t, _)| t.clone()).collect();
        assert_eq!(tags_in_order, vec!["base", "pandas", "sklearn"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- Phase 1 unit tests (3/7): missing parent --------------------

    #[test]
    fn resolve_chain_missing_parent_actionable_error() {
        let dir = make_base_and_diff("missing", 0xAA, 0xBB);
        let pandas = snap_at(&dir.join("diff.bin"), Some("nonexistent-base"), Some("h"));
        let mut tags = HashMap::new();
        tags.insert("pandas".to_string(), pandas);

        let err = resolve_chain("pandas", |t| tags.get(t).cloned().context("missing")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("nonexistent-base") && msg.contains("pandas"),
            "error must name both the missing parent and the dependent: {msg}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- Phase 1 unit tests (4/7): cycle detection -------------------

    #[test]
    fn resolve_chain_cycle_detected() {
        let dir = make_base_and_diff("cycle", 0xAA, 0xBB);
        // Pathological: a -> b -> a. This shouldn't happen via
        // `forkd snapshot diff`, but a hand-edited snapshot.json
        // or a future bug could produce it.
        let a = snap_at(&dir.join("diff.bin"), Some("b"), Some("h"));
        let b = snap_at(&dir.join("diff.bin"), Some("a"), Some("h"));
        let mut tags = HashMap::new();
        tags.insert("a".to_string(), a);
        tags.insert("b".to_string(), b);

        let err = resolve_chain("a", |t| tags.get(t).cloned().context("missing")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("cycle"),
            "cycle error must mention 'cycle': {msg}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- Phase 1 unit tests (5/7): hash mismatch ---------------------

    #[test]
    fn verify_parent_hash_mismatch_actionable_error() {
        let dir = make_base_and_diff("hash-mismatch", 0xAA, 0xBB);
        let base = snap_at(&dir.join("base.bin"), None, None);
        // Wrong hash on purpose — base's actual sha256 of 16 KiB of
        // 0xAA isn't this.
        let bogus = "deadbeef".repeat(8);
        let pandas = snap_at(&dir.join("diff.bin"), Some("base"), Some(&bogus));
        let chain = vec![("base".to_string(), base), ("pandas".to_string(), pandas)];

        let err = verify_parent_hashes(&chain).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("pandas") && msg.contains("base") && msg.contains("rebuild"),
            "hash-mismatch error must name child, parent, and a remediation: {msg}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- Phase 1 unit tests (6/7): hash match passes -----------------

    #[test]
    fn verify_parent_hash_match_passes() {
        let dir = make_base_and_diff("hash-match", 0xAA, 0xBB);
        let base_path = dir.join("base.bin");
        let actual = sha256_file(&base_path).unwrap();
        let base = snap_at(&base_path, None, None);
        let pandas = snap_at(&dir.join("diff.bin"), Some("base"), Some(&actual));
        let chain = vec![("base".to_string(), base), ("pandas".to_string(), pandas)];

        verify_parent_hashes(&chain).expect("hash match should pass");
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- Phase 1 unit tests (7/7): assemble correctness --------------

    #[test]
    fn assemble_chain_memory_produces_correct_bytes() {
        let dir = make_base_and_diff("assemble", 0xAA, 0xBB);
        let base = snap_at(&dir.join("base.bin"), None, None);
        // We don't run verify_parent_hashes() in this test, so a
        // placeholder "h" suffices for the parent_content_hash slot.
        let pandas = snap_at(&dir.join("diff.bin"), Some("base"), Some("h"));
        let chain = vec![("base".to_string(), base), ("pandas".to_string(), pandas)];

        let out_path = dir.join("assembled.bin");
        let total = assemble_chain_memory(&chain, &out_path).unwrap();

        // The base copy is the dominant contribution; the diff
        // overlay contributes 4 KiB (the one data region).
        assert!(
            total >= 16 * 1024,
            "total reported {total} must include at least the base bytes"
        );

        // Read back and verify content. First 4 KiB stays 0xAA (base
        // only); next 4 KiB becomes 0xBB (overlaid by the diff);
        // remaining 8 KiB stays 0xAA.
        let bytes = std::fs::read(&out_path).unwrap();
        assert_eq!(bytes.len(), 16 * 1024);
        assert!(
            bytes[0..4096].iter().all(|&b| b == 0xAA),
            "first 4 KiB should be base (0xAA)"
        );
        assert!(
            bytes[4096..8192].iter().all(|&b| b == 0xBB),
            "second 4 KiB should be diff overlay (0xBB)"
        );
        assert!(
            bytes[8192..].iter().all(|&b| b == 0xAA),
            "remaining 8 KiB should be base (0xAA) — diff was a hole there"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    // Bonus: assemble bails if the output path is already taken (the
    // caller-owns-scratch contract). Belt-and-suspenders.

    #[test]
    fn assemble_chain_memory_refuses_to_clobber_existing() {
        let dir = make_base_and_diff("no-clobber", 0xAA, 0xBB);
        let base = snap_at(&dir.join("base.bin"), None, None);
        let chain = vec![("base".to_string(), base)];

        let out_path = dir.join("already-here.bin");
        std::fs::write(&out_path, b"don't overwrite me").unwrap();

        let err = assemble_chain_memory(&chain, &out_path).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("already exists"), "got: {msg}");
        // Output is intact.
        assert_eq!(std::fs::read(&out_path).unwrap(), b"don't overwrite me");
        std::fs::remove_dir_all(&dir).ok();
    }
}
