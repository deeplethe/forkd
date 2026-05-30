//! `memfd_create(2)`-backed memory regions for the v0.4 live-fork path.
//!
//! Concretely, [`create_and_populate`] takes a path to a snapshot's
//! `memory.bin`, copies the bytes into a fresh anonymous file (memfd),
//! and returns a [`MemfdRegion`] that holds the file alive and exposes
//! `/proc/self/fd/<N>` as a path the Firecracker controller can hand to
//! the patched FC via `mem_backend.backend_path` with `shared: true`
//! (see [`docs/VENDORED-FIRECRACKER.md`](../../../docs/VENDORED-FIRECRACKER.md)
//! for the FC-side change).
//!
//! Why memfd instead of the original file:
//!
//! - `UFFDIO_WRITEPROTECT` (the kernel primitive v0.4 uses to capture
//!   dirty pages out-of-band) supports anonymous and shmem VMAs but not
//!   arbitrary file-backed mappings. `memfd_create` produces a shmem
//!   inode, which qualifies.
//! - Holding the memfd in `forkd-controller` lets the controller mmap
//!   the same backing pages as the FC child. When FC mmaps with
//!   `MAP_SHARED` (the path the vendored patch enables), guest writes
//!   are visible to the controller's view of the region.
//! - The memfd dies with the fd. Once `forkd-controller` drops the
//!   `MemfdRegion`, the kernel reclaims the pages immediately — no
//!   stale file on disk.
//!
//! Linux-only because `memfd_create` is a Linux syscall. On other
//! targets this module's public surface returns errors so callers don't
//! silently fall back to file-backed semantics.

use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// A memfd populated from a snapshot's memory file. Dropping the value
/// closes the fd and releases the backing pages.
///
/// Pass [`MemfdRegion::backend_path`] to Firecracker as
/// `mem_backend.backend_path`; the patched FC will open it via
/// `/proc/<our_pid>/fd/<N>` (after `dup`-ing the inode) and mmap with
/// `MAP_SHARED` when `mem_backend.shared` is `true`.
#[derive(Debug)]
pub struct MemfdRegion {
    #[cfg(target_os = "linux")]
    file: File,
    size_bytes: u64,
}

impl MemfdRegion {
    /// Logical size of the region in bytes.
    pub fn size_bytes(&self) -> u64 {
        self.size_bytes
    }

    /// `/proc/<controller-pid>/fd/<N>` path Firecracker can pass to
    /// `mem_backend.backend_path`. Stable for the lifetime of `self`.
    ///
    /// Uses the explicit controller PID rather than `self` because FC
    /// opens this path from its own process — `/proc/self/fd/N` would
    /// resolve against FC's fd table and miss. Caught in E2E (Phase 6
    /// memfd_handle path); see DESIGN-v0.4-PHASE6.md.
    #[cfg(target_os = "linux")]
    pub fn backend_path(&self) -> PathBuf {
        PathBuf::from(format!(
            "/proc/{}/fd/{}",
            std::process::id(),
            self.file.as_raw_fd()
        ))
    }

    /// Return a duplicated `File` handle pointing at the same memfd.
    /// Useful for tests and for callers that want to mmap the region
    /// directly. Caller owns the new fd and must drop it.
    #[cfg(target_os = "linux")]
    pub fn try_clone(&self) -> io::Result<File> {
        self.file.try_clone()
    }
}

/// Create a memfd, size it to the source file's length, and copy the
/// source bytes in.
///
/// `name` is recorded with the memfd (visible as the file's name in
/// `/proc/self/fd/<N>` -> `target`); keep it short and ASCII. The
/// kernel limit is 249 bytes plus the `memfd:` prefix.
///
/// Returns `Err` immediately if the source is missing or unreadable —
/// no partial memfd is created in that case.
#[cfg(target_os = "linux")]
pub fn create_and_populate(source: &Path, name: &str) -> Result<MemfdRegion> {
    use std::io::copy;
    use std::os::unix::io::FromRawFd;

    let mut src =
        File::open(source).with_context(|| format!("open memfd source {}", source.display()))?;
    let size_bytes = src
        .metadata()
        .with_context(|| format!("stat memfd source {}", source.display()))?
        .len();

    let cname = CString::new(name).context("memfd name must not contain null bytes")?;
    // SAFETY: `cname` is a valid C string for the duration of the call;
    // memfd_create either returns a fresh owned fd or -1. Flags are a
    // literal bitfield. No aliasing concerns.
    let fd = unsafe { libc::memfd_create(cname.as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error()).context("memfd_create");
    }
    // SAFETY: `fd` is freshly returned by memfd_create above and not
    // shared with any other File. `File::from_raw_fd` takes ownership.
    let mut memfd = unsafe { File::from_raw_fd(fd) };
    memfd
        .set_len(size_bytes)
        .with_context(|| format!("ftruncate memfd to {size_bytes} B"))?;

    let copied = copy(&mut src, &mut memfd)
        .with_context(|| format!("copy {} -> memfd", source.display()))?;
    if copied != size_bytes {
        anyhow::bail!(
            "short copy: source {} is {size_bytes} B but copied {copied}",
            source.display()
        );
    }

    Ok(MemfdRegion {
        file: memfd,
        size_bytes,
    })
}

/// Non-Linux stub. `memfd_create` is a Linux-only syscall; building
/// forkd on other platforms is a configuration error for the v0.4
/// live-fork path.
#[cfg(not(target_os = "linux"))]
pub fn create_and_populate(_source: &Path, _name: &str) -> Result<MemfdRegion> {
    anyhow::bail!(
        "memfd_create is Linux-only; v0.4 live-fork requires a Linux host with kernel >= 5.7"
    )
}

#[cfg(target_os = "linux")]
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Seek, SeekFrom, Write};

    fn write_temp_file(label: &str, content: &[u8]) -> PathBuf {
        let p =
            std::env::temp_dir().join(format!("memfd-test-{}-{}.bin", label, std::process::id()));
        let mut f = File::create(&p).unwrap();
        f.write_all(content).unwrap();
        p
    }

    #[test]
    fn create_and_populate_succeeds_for_small_file() {
        let src = write_temp_file("small", &vec![0xAAu8; 4096]);
        let region = create_and_populate(&src, "forkd-test-small").unwrap();
        assert_eq!(region.size_bytes(), 4096);
        let p = region.backend_path();
        let s = p.to_str().unwrap();
        assert!(
            s.starts_with("/proc/self/fd/"),
            "expected /proc/self/fd/N path, got: {s}"
        );
        let _ = std::fs::remove_file(&src);
    }

    #[test]
    fn populated_memfd_content_matches_source() {
        // Use a pattern that catches off-by-one and wrong-direction copy
        // bugs (sequential bytes mod 256, 8 KiB worth).
        let pattern: Vec<u8> = (0..8192).map(|i| (i % 256) as u8).collect();
        let src = write_temp_file("match", &pattern);

        let region = create_and_populate(&src, "forkd-test-match").unwrap();
        assert_eq!(region.size_bytes(), 8192);

        let mut reader = region.try_clone().unwrap();
        reader.seek(SeekFrom::Start(0)).unwrap();
        let mut buf = vec![0u8; 8192];
        reader.read_exact(&mut buf).unwrap();
        assert_eq!(buf, pattern, "memfd content must match source");

        let _ = std::fs::remove_file(&src);
    }

    #[test]
    fn missing_source_file_errors() {
        let result = create_and_populate(
            Path::new("/nonexistent/forkd-memfd-test/this-must-not-exist"),
            "forkd-test-missing",
        );
        assert!(
            result.is_err(),
            "should fail early when source file doesn't exist"
        );
        // And the error should mention the source path so the operator
        // knows which file the daemon couldn't find.
        let msg = format!("{:#}", result.unwrap_err());
        assert!(
            msg.contains("this-must-not-exist"),
            "error must include source path; got: {msg}"
        );
    }
}
