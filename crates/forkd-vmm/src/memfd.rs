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

// log2(2 MiB) = 21, shifted into the hugepage-size field per <linux/memfd.h>.
#[cfg(target_os = "linux")]
const MFD_HUGE_2MB: libc::c_uint = 21 << libc::MFD_HUGE_SHIFT;

// 2 MiB in bytes — ftruncate on a hugetlb memfd requires size to be a multiple of this.
#[cfg(target_os = "linux")]
const HUGE_PAGE_2MB: u64 = 2 * 1024 * 1024;

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
/// `use_hugepages` is a boolean flag that when turned on activates the
/// MFD_HUGETLB & MFD_HUGE_2MB flags - backing the guest RAM with 2MiB
/// pages. If hugepage allocation fails (usually due to `ENOMEM` where
/// we have exhausted the hugepage pool), we fallback to default behavior.
///
/// Returns `Err` immediately if the source is missing or unreadable —
/// no partial memfd is created in that case.
#[cfg(target_os = "linux")]
pub fn create_and_populate(source: &Path, name: &str, use_hugepages: bool) -> Result<MemfdRegion> {
    use std::io::copy;
    use std::os::unix::io::FromRawFd;

    let mut src =
        File::open(source).with_context(|| format!("open memfd source {}", source.display()))?;
    let size_bytes = src
        .metadata()
        .with_context(|| format!("stat memfd source {}", source.display()))?
        .len();

    let cname = CString::new(name).context("memfd name must not contain null bytes")?;

    // Attempt hugepage-backed allocation if requested; normal pages otherwise.
    let (fd, alloc_size, backed_by_hugepages) = if use_hugepages {
        let aligned_size = (size_bytes + HUGE_PAGE_2MB - 1) & !(HUGE_PAGE_2MB - 1);
        // SAFETY: `cname` is a valid C string for the duration of the call;
        // memfd_create returns a fresh owned fd or -1. Flags are a literal bitfield.
        let fd = unsafe {
            libc::memfd_create(
                cname.as_ptr(),
                libc::MFD_CLOEXEC | libc::MFD_HUGETLB | MFD_HUGE_2MB,
            )
        };
        (fd, aligned_size, true)
    } else {
        // SAFETY: same as above.
        let fd = unsafe { libc::memfd_create(cname.as_ptr(), libc::MFD_CLOEXEC) };
        (fd, size_bytes, false)
    };

    // Handle allocation failure. For hugepages, ENOMEM means the pool is
    // exhausted - warn and retry with normal 4 KiB pages. Any other error,
    // or a failure on the normal path, is fatal.
    let (fd, alloc_size, backed_by_hugepages) = if fd < 0 {
        let err = io::Error::last_os_error();
        if backed_by_hugepages && err.raw_os_error() == Some(libc::ENOMEM) {
            // hugepage allocation failure
            tracing::warn!(
                "hugepage pool exhausted (HugePages_Free=0?); \
                 falling back to normal 4 KiB pages for memfd '{name}'. \
                 Increase /proc/sys/vm/nr_hugepages to suppress this."
            );
            // SAFETY: same as above — fresh syscall, no aliasing.
            let fd = unsafe { libc::memfd_create(cname.as_ptr(), libc::MFD_CLOEXEC) };
            if fd < 0 {
                return Err(io::Error::last_os_error()).context("memfd_create (fallback)");
            }
            (fd, size_bytes, false)
        } else {
            // some other unknown error
            return Err(err).context("memfd_create");
        }
    } else {
        // no error
        (fd, alloc_size, backed_by_hugepages)
    };

    // SAFETY: `fd` is freshly returned by memfd_create above and not
    // shared with any other File. `File::from_raw_fd` takes ownership.
    let mut memfd = unsafe { File::from_raw_fd(fd) };
    memfd
        .set_len(alloc_size)
        .with_context(|| format!("ftruncate memfd to {alloc_size} B"))?;

    // Hugetlb-backed memfds don't support write(), must use copy_via_mmap
    let copied = if backed_by_hugepages {
        copy_via_mmap(&src, &memfd, size_bytes, alloc_size)
            .with_context(|| format!("copy (mmap) {} -> memfd", source.display()))?
    } else {
        copy(&mut src, &mut memfd).with_context(|| format!("copy {} -> memfd", source.display()))?
    };
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

/// Populate a hugetlb-backed memfd from a source file via mmap + memcpy.
///
/// HugeTLB files don't support write(). This method populates these pages
/// by mmp the memfd MAP_SHARED, mmap the source file MAP_PRIVATE, and memcpy
/// between the two mappings.
///
/// # Parameters
/// - `src`: the source file (`memory.bin`) to copy from.
/// - `dst`: the hugetlb-backed memfd to copy into. Must already be sized
///   via `set_len(alloc_size)` before calling.
/// - `size_bytes`: exact number of bytes to copy from `src`. Must be <=
///   `alloc_size`.
/// - `alloc_size`: the memfd's `ftruncate`'d size (hugepage-aligned,
///   >= `size_bytes`). Used as the mmap length for `dst`.
///
/// # Returns
/// The number of bytes copied (always equal to `size_bytes` on success).
#[cfg(target_os = "linux")]
fn copy_via_mmap(src: &File, dst: &File, size_bytes: u64, alloc_size: u64) -> io::Result<u64> {
    use std::os::fd::AsRawFd;

    if size_bytes > alloc_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("size_bytes ({size_bytes}) must be <= alloc_size ({alloc_size})"),
        ));
    }

    // dst is mapped at alloc_size (hugepage-aligned) but only size_bytes of
    // actual data is copied into it. The tail (alloc_size - size_bytes) stays
    // as the post-ftruncate zero-fill. FC never reads past size_bytes since
    // the VMM API call passes size_bytes as the memory region length.
    //
    // SAFETY: dst is an open memfd sized to alloc_size via set_len; we own
    // it and munmap before returning. MAP_SHARED so the memcpy below lands
    // in the fd's backing pages rather than a private anonymous copy.
    let dst_ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            alloc_size as usize,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            dst.as_raw_fd(),
            0,
        )
    };
    if dst_ptr == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }

    // SAFETY: src is an open readable file of at least size_bytes; we own
    // it and munmap before returning. MAP_PRIVATE so reads don't affect
    // the source file's contents.
    let src_ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            size_bytes as usize,
            libc::PROT_READ,
            libc::MAP_PRIVATE,
            src.as_raw_fd(),
            0,
        )
    };
    if src_ptr == libc::MAP_FAILED {
        // SAFETY: dst_ptr is a valid mapping we created above.
        unsafe { libc::munmap(dst_ptr, alloc_size as usize) };
        return Err(io::Error::last_os_error());
    }

    // SAFETY: both pointers are valid for the given lengths and don't overlap
    // (they came from separate mmap calls on different fds).
    unsafe {
        std::ptr::copy_nonoverlapping(
            src_ptr as *const u8,
            dst_ptr as *mut u8,
            size_bytes as usize,
        );
    }

    // SAFETY: both pointers are valid mappings we created above with the
    // exact lengths passed here.
    unsafe {
        libc::munmap(src_ptr, size_bytes as usize);
        libc::munmap(dst_ptr, alloc_size as usize);
    }

    Ok(size_bytes)
}

/// Non-Linux stub. `memfd_create` is a Linux-only syscall; building
/// forkd on other platforms is a configuration error for the v0.4
/// live-fork path.
#[cfg(not(target_os = "linux"))]
pub fn create_and_populate(
    _source: &Path,
    _name: &str,
    _use_hugepages: bool,
) -> Result<MemfdRegion> {
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
        let region = create_and_populate(&src, "forkd-test-small", false).unwrap();
        assert_eq!(region.size_bytes(), 4096);
        let p = region.backend_path();
        let s = p.to_str().unwrap();
        // backend_path() embeds the explicit controller PID (not "self")
        // because FC opens this path from its own process; see the
        // doc comment on backend_path for why.
        let expected_prefix = format!("/proc/{}/fd/", std::process::id());
        assert!(
            s.starts_with(&expected_prefix),
            "expected {expected_prefix}N path, got: {s}"
        );
        let _ = std::fs::remove_file(&src);
    }

    #[test]
    fn populated_memfd_content_matches_source() {
        // Use a pattern that catches off-by-one and wrong-direction copy
        // bugs (sequential bytes mod 256, 8 KiB worth).
        let pattern: Vec<u8> = (0..8192).map(|i| (i % 256) as u8).collect();
        let src = write_temp_file("match", &pattern);

        let region = create_and_populate(&src, "forkd-test-match", false).unwrap();
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
            false,
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

    // --- copy_via_mmap unit tests (no hugepages required) ---

    #[test]
    fn copy_via_mmap_size_guard_rejects_oversized_request() {
        // size_bytes > alloc_size must return an error immediately.
        // Use /dev/zero as a stand-in fd — we never reach the mmap calls.
        let zero = File::open("/dev/zero").unwrap();
        let err = copy_via_mmap(&zero, &zero, 8192, 4096).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        let msg = err.to_string();
        assert!(
            msg.contains("size_bytes") && msg.contains("alloc_size"),
            "error must name both fields; got: {msg}"
        );
    }

    #[test]
    fn copy_via_mmap_content_matches() {
        use std::os::unix::io::FromRawFd;

        // Build a source file with a known pattern.
        let pattern: Vec<u8> = (0..4096).map(|i| (i % 256) as u8).collect();
        let src = write_temp_file("mmap-src", &pattern);
        let src_file = File::open(&src).unwrap();

        // Create a plain (non-hugetlb) memfd as the destination.
        let name = std::ffi::CString::new("forkd-mmap-test").unwrap();
        let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
        assert!(fd >= 0, "memfd_create failed");
        let dst_file = unsafe { File::from_raw_fd(fd) };
        dst_file.set_len(4096).unwrap();

        let copied = copy_via_mmap(&src_file, &dst_file, 4096, 4096).unwrap();
        assert_eq!(copied, 4096);

        // Read back through the fd and verify content.
        let mut reader = dst_file.try_clone().unwrap();
        reader.seek(SeekFrom::Start(0)).unwrap();
        let mut buf = vec![0u8; 4096];
        reader.read_exact(&mut buf).unwrap();
        assert_eq!(buf, pattern, "mmap copy must produce identical bytes");

        let _ = std::fs::remove_file(&src);
    }

    // --- hugepages tests (skipped gracefully when pool unavailable) ---

    fn hugepages_available() -> bool {
        std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find(|l| l.starts_with("HugePages_Free:"))
                    .and_then(|l| l.split_whitespace().nth(1))
                    .and_then(|v| v.parse::<u64>().ok())
            })
            .map(|n| n > 0)
            .unwrap_or(false)
    }

    #[test]
    fn hugepages_metadata_correct() {
        if !hugepages_available() {
            eprintln!(
                "skipping hugepages_metadata_correct: HugePages_Free=0 \
                       (run `echo 512 | sudo tee /proc/sys/vm/nr_hugepages` to enable)"
            );
            return;
        }
        let src = write_temp_file("hp-meta", &vec![0xBBu8; 4096]);
        let region = create_and_populate(&src, "forkd-test-hp-meta", true).unwrap();

        assert_eq!(
            region.size_bytes(),
            4096,
            "size_bytes must reflect source, not alloc_size"
        );

        let p = region.backend_path();
        let s = p.to_str().unwrap();
        let expected_prefix = format!("/proc/{}/fd/", std::process::id());
        assert!(
            s.starts_with(&expected_prefix),
            "expected {expected_prefix}N path, got: {s}"
        );
        let _ = std::fs::remove_file(&src);
    }

    #[test]
    fn hugepages_content_matches_source() {
        if !hugepages_available() {
            eprintln!(
                "skipping hugepages_content_matches_source: HugePages_Free=0 \
                       (run `echo 512 | sudo tee /proc/sys/vm/nr_hugepages` to enable)"
            );
            return;
        }
        let pattern: Vec<u8> = (0..8192).map(|i| (i % 256) as u8).collect();
        let src = write_temp_file("hp-content", &pattern);

        let region = create_and_populate(&src, "forkd-test-hp-content", true).unwrap();
        assert_eq!(region.size_bytes(), 8192);

        let mut reader = region.try_clone().unwrap();
        reader.seek(SeekFrom::Start(0)).unwrap();
        let mut buf = vec![0u8; 8192];
        reader.read_exact(&mut buf).unwrap();
        assert_eq!(
            buf, pattern,
            "hugepage-backed memfd content must match source"
        );

        let _ = std::fs::remove_file(&src);
    }

    #[test]
    fn hugepages_size_bytes_is_source_size_not_aligned() {
        if !hugepages_available() {
            eprintln!(
                "skipping hugepages_size_bytes_is_source_size_not_aligned: HugePages_Free=0 \
                       (run `echo 512 | sudo tee /proc/sys/vm/nr_hugepages` to enable)"
            );
            return;
        }
        // 4096 bytes is well below 2 MiB — alloc_size will be rounded up to
        // 2 MiB, but size_bytes() must still return 4096.
        let src = write_temp_file("hp-align", &vec![0xCCu8; 4096]);
        let region = create_and_populate(&src, "forkd-test-hp-align", true).unwrap();

        assert_eq!(
            region.size_bytes(),
            4096,
            "size_bytes must be the source size (4096), not the hugepage-aligned alloc_size ({})",
            HUGE_PAGE_2MB,
        );
        let _ = std::fs::remove_file(&src);
    }
}
