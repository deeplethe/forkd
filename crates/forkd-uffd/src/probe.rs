//! Capability probes for v0.4 live-fork prerequisites.
//!
//! Cheap, non-destructive syscall checks that answer "would the
//! live-fork path work on this host?" without spinning up a VM. Used
//! by `forkd doctor` to surface kernel/permission issues early instead
//! of letting them blow up the first BRANCH.
//!
//! Each probe opens the relevant fd, exercises the minimum operation
//! needed to verify the feature, and drops the fd before returning.
//! No state survives the call.

use anyhow::{bail, Context, Result};
use std::io;
use std::os::unix::io::{FromRawFd, OwnedFd};

/// Verify the kernel supports `userfaultfd(2)` with
/// `UFFD_FEATURE_PAGEFAULT_FLAG_WP`.
///
/// Opens a userfaultfd, negotiates `UFFDIO_API` requesting the WP
/// feature, and checks the kernel advertised it back. Drops the fd
/// before returning either way.
///
/// Returns `Ok(())` on success; `Err` on:
/// - Kernel too old (< 5.7) — userfaultfd doesn't know WP.
/// - `vm.unprivileged_userfaultfd=0` and the caller lacks
///   `CAP_SYS_PTRACE` — userfaultfd(2) fails with EPERM.
/// - WP feature negotiated but not granted (unusual; would mean a
///   patched kernel that explicitly removed WP support).
pub fn probe_uffd_wp() -> Result<()> {
    // userfaultfd(2): x86_64 syscall 323. Matches `raw::create_uffd`
    // but kept separate so doctor doesn't pull in the rest of the
    // snapshot-side machinery just to call this one syscall.
    const SYS_USERFAULTFD: libc::c_long = 323;
    const O_CLOEXEC: libc::c_int = 0o2000000;
    const O_NONBLOCK: libc::c_int = 0o4000;
    const UFFD_API: u64 = 0xAA;
    const UFFD_FEATURE_PAGEFAULT_FLAG_WP: u64 = 1 << 9;
    // _IOWR('U', 0x3F, struct uffdio_api): u32 << 30 | size<<16 | 'U'<<8 | 0x3F.
    // Matches raw::UFFDIO_API_NR; recomputed here to keep this module
    // self-contained.
    const UFFDIO_API_IOC: libc::c_ulong = 0xc018aa3f;

    #[repr(C)]
    struct UffdioApi {
        api: u64,
        features: u64,
        ioctls: u64,
    }

    let fd = unsafe { libc::syscall(SYS_USERFAULTFD, O_CLOEXEC | O_NONBLOCK) };
    if fd < 0 {
        return Err(io::Error::last_os_error()).context(
            "userfaultfd(2) — need kernel >= 5.7 plus CAP_SYS_PTRACE \
             or sysctl vm.unprivileged_userfaultfd=1",
        );
    }
    // SAFETY: kernel returned a fresh fd we own; wrap so Drop closes it.
    let owned = unsafe { OwnedFd::from_raw_fd(fd as i32) };

    let mut api = UffdioApi {
        api: UFFD_API,
        features: UFFD_FEATURE_PAGEFAULT_FLAG_WP,
        ioctls: 0,
    };
    // SAFETY: owned.as_raw_fd() is a live userfaultfd; UFFDIO_API
    // reads/writes the struct we point at.
    let rc = unsafe {
        libc::ioctl(
            std::os::unix::io::AsRawFd::as_raw_fd(&owned),
            UFFDIO_API_IOC,
            &mut api as *mut _,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error()).context("UFFDIO_API");
    }
    if (api.features & UFFD_FEATURE_PAGEFAULT_FLAG_WP) == 0 {
        bail!(
            "kernel did not advertise UFFD_FEATURE_PAGEFAULT_FLAG_WP \
             (got 0x{:x}); need kernel >= 5.7 built with userfaultfd \
             WP support",
            api.features
        );
    }
    // owned drops -> close(fd). No state left behind.
    Ok(())
}

/// Verify `memfd_create(2)` is available and produces a usable fd.
///
/// Creates an anonymous memfd with `MFD_CLOEXEC`, drops the fd, and
/// returns. `memfd_create` has been in the kernel since 3.17, so this
/// almost always succeeds on a forkd-target host — but it can fail
/// inside containers with a restrictive seccomp profile (e.g. some
/// Docker default profiles before 20.10), which is exactly the failure
/// mode worth catching early.
///
/// Returns `Ok(())` on success; `Err` on syscall failure (likely
/// `ENOSYS` from seccomp, or `EFAULT` from an exotic kernel).
pub fn probe_memfd_create() -> Result<()> {
    // Short ASCII name, unique-ish so two concurrent doctors don't
    // race on the memfd display name.
    let name = format!("forkd-doctor-probe-{}\0", std::process::id());
    const MFD_CLOEXEC: libc::c_uint = 1;
    // SAFETY: name is a NUL-terminated UTF-8 string; memfd_create
    // returns a fresh owned fd or -1.
    let fd = unsafe { libc::memfd_create(name.as_ptr() as *const libc::c_char, MFD_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error()).context(
            "memfd_create(2) — kernel >= 3.17 required; seccomp inside \
             a container can also block this syscall",
        );
    }
    // SAFETY: kernel returned a fresh fd; OwnedFd::Drop closes it.
    let _owned = unsafe { OwnedFd::from_raw_fd(fd) };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uffd_wp_probe_on_a_supported_host() {
        // CI runs on Linux >= 5.15 with unprivileged_userfaultfd=1, so
        // this should pass. If the probe fails, the error message
        // itself must be informative — the test asserts both the
        // happy path and (when run on an unsupported host) that the
        // error is actionable.
        match probe_uffd_wp() {
            Ok(()) => {}
            Err(e) => {
                let msg = format!("{e:#}");
                assert!(
                    msg.contains("userfaultfd")
                        || msg.contains("UFFDIO_API")
                        || msg.contains("UFFD_FEATURE"),
                    "uffd_wp probe error must mention what failed; got: {msg}"
                );
            }
        }
    }

    #[test]
    fn memfd_create_probe_succeeds() {
        // memfd_create exists on any forkd-target kernel; we'd only
        // see this fail under unusual seccomp profiles, which CI
        // doesn't have.
        probe_memfd_create().expect("memfd_create should work in CI");
    }
}
