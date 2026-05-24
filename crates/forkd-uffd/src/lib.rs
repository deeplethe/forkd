//! forkd uffd handler — library half.
//!
//! Implements the Firecracker UDS handshake (receive uffd fd + memory
//! region descriptors over `SCM_RIGHTS`) and a no-op event-loop stub.
//! See `docs/design/userfaultfd.md` at the repo root for the v0.3 design.
//!
//! **Phase 1 scope.** This crate currently completes the handshake and
//! exits cleanly (or runs a debug-only "log each fault, never serve it"
//! loop). The real `UFFDIO_COPY` event loop lands in phase 3 once we
//! also have the memfd-backed source RAM path from phase 2.

use serde::{Deserialize, Serialize};

#[cfg(target_os = "linux")]
pub(crate) mod raw;

#[cfg(target_os = "linux")]
pub mod wp_snapshot;

/// One contiguous chunk of guest physical memory that Firecracker has
/// mapped in its own process and registered with the userfaultfd.
///
/// Wire-compatible with Firecracker's `GuestRegionUffdMapping`
/// (`src/firecracker/examples/uffd/uffd_utils.rs` in
/// firecracker v1.10.1). When Firecracker connects to our UDS at
/// snapshot-load time it sends one JSON-encoded `Vec<GuestRegionUffdMapping>`
/// (this struct) plus the uffd file descriptor as `SCM_RIGHTS`
/// ancillary data.
///
/// On x86_64 with `mem_size_mib` ≤ ~3 GiB the layout is a single region.
/// Above that the PCI hole splits guest RAM into two regions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GuestRegionUffdMapping {
    /// Host virtual address where Firecracker mapped this guest region.
    /// uffd events arrive in terms of host VAs; the handler subtracts
    /// `base_host_virt_addr` from the fault address to get an offset
    /// into the backing memory.bin.
    pub base_host_virt_addr: u64,
    /// Size of the region in bytes. Always a multiple of `page_size_kib * 1024`.
    pub size: usize,
    /// Byte offset of this region within the backing `memory.bin`.
    /// Multiple regions cover disjoint offset ranges that together span
    /// the whole memory.bin.
    pub offset: u64,
    /// Page size of this region in KiB. Firecracker uses 4 KiB pages
    /// today; the field exists for future huge-page support.
    pub page_size_kib: usize,
}

impl GuestRegionUffdMapping {
    /// Convenience: does a host VA fall inside this region?
    pub fn contains(&self, host_va: u64) -> bool {
        host_va >= self.base_host_virt_addr && host_va < self.base_host_virt_addr + self.size as u64
    }

    /// Translate a host VA back to the byte offset within memory.bin.
    /// Returns None if `host_va` is outside this region.
    pub fn offset_for(&self, host_va: u64) -> Option<u64> {
        if !self.contains(host_va) {
            return None;
        }
        Some(self.offset + (host_va - self.base_host_virt_addr))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn region_contains_addresses_within_bounds() {
        let r = GuestRegionUffdMapping {
            base_host_virt_addr: 0x1000,
            size: 0x4000,
            offset: 0,
            page_size_kib: 4,
        };
        assert!(r.contains(0x1000));
        assert!(r.contains(0x4FFF));
        assert!(!r.contains(0xFFF));
        assert!(!r.contains(0x5000));
    }

    #[test]
    fn region_offset_for_translates_host_va_to_memory_bin_offset() {
        let r = GuestRegionUffdMapping {
            base_host_virt_addr: 0x10000,
            size: 0x4000,
            offset: 0x100, // this region starts 256B into memory.bin
            page_size_kib: 4,
        };
        // First page of the region maps to memory.bin offset 0x100.
        assert_eq!(r.offset_for(0x10000), Some(0x100));
        // 4 KiB in maps to memory.bin offset 0x1100.
        assert_eq!(r.offset_for(0x11000), Some(0x1100));
        // Outside the region returns None.
        assert_eq!(r.offset_for(0x9000), None);
    }

    #[test]
    fn region_serde_round_trip_matches_firecracker_wire_format() {
        // Field names and casing must match Firecracker's serialization
        // verbatim — Firecracker uses serde defaults (snake_case). Any
        // rename here breaks the handshake silently.
        let r = GuestRegionUffdMapping {
            base_host_virt_addr: 0x7f00_0000_0000,
            size: 0x4000_0000, // 1 GiB
            offset: 0,
            page_size_kib: 4,
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("base_host_virt_addr"));
        assert!(json.contains("size"));
        assert!(json.contains("offset"));
        assert!(json.contains("page_size_kib"));
        let back: GuestRegionUffdMapping = serde_json::from_str(&json).unwrap();
        assert_eq!(r, back);
    }
}

#[cfg(target_os = "linux")]
pub mod handshake {
    //! Receive `(uffd_fd, Vec<GuestRegionUffdMapping>)` from Firecracker
    //! over a unix-domain socket using `recvmsg` + `SCM_RIGHTS`.
    //!
    //! Layered as a separate module so non-Linux builds (developer
    //! laptops on macOS / Windows) can still depend on the crate for the
    //! `GuestRegionUffdMapping` type and round-trip tests.

    use super::GuestRegionUffdMapping;
    use anyhow::{bail, Context, Result};
    use std::io::IoSliceMut;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::Path;

    /// What we received from Firecracker in one `recvmsg` call.
    pub struct Handshake {
        /// Owned userfaultfd file descriptor. Drop will close it.
        pub uffd: OwnedFd,
        /// Memory regions Firecracker registered. Each event the uffd
        /// emits will fall inside exactly one of these regions.
        pub regions: Vec<GuestRegionUffdMapping>,
    }

    /// Listen on `sock_path`, accept one connection from Firecracker,
    /// receive the handshake, and return it.
    ///
    /// This binds the socket inside the function — `sock_path` must not
    /// already exist. Caller is responsible for choosing a path inside
    /// a directory only the handler and Firecracker can reach (typically
    /// `forkd-controller`'s work_dir).
    pub fn accept_handshake(sock_path: &Path) -> Result<Handshake> {
        if sock_path.exists() {
            // Stale socket from a previous run — best-effort remove.
            let _ = std::fs::remove_file(sock_path);
        }
        let listener = UnixListener::bind(sock_path)
            .with_context(|| format!("bind {}", sock_path.display()))?;
        let (stream, _) = listener.accept().context("accept firecracker connection")?;
        recv_handshake(&stream)
    }

    /// Same as [`accept_handshake`] but works on an already-connected
    /// stream — useful for tests that pair a fake firecracker with the
    /// real handler over a `socketpair`.
    pub fn recv_handshake(stream: &UnixStream) -> Result<Handshake> {
        // Buffer sized for the JSON payload. Firecracker's payload for
        // a typical 4 GiB VM with the PCI-hole split is ~200 bytes; 4 KiB
        // is comfortably larger than any realistic region descriptor list.
        let mut buf = vec![0u8; 4096];
        let mut iov = [IoSliceMut::new(&mut buf)];

        // Ancillary buffer for one fd via SCM_RIGHTS. `CMSG_SPACE` for
        // a single i32 fd is small; allocate 64 bytes to be safe.
        let mut cmsg_space = [0u8; 64];

        // SAFETY: we construct a msghdr with valid pointers into stack
        // buffers and call `recvmsg(2)` directly via libc. The Rust
        // standard library doesn't expose recvmsg for unix sockets, and
        // pulling in `nix` is a heavy dep for one syscall.
        let (n_data, fd) = unsafe {
            let mut msg: libc::msghdr = std::mem::zeroed();
            msg.msg_iov = iov.as_mut_ptr() as *mut libc::iovec;
            msg.msg_iovlen = iov.len() as _;
            msg.msg_control = cmsg_space.as_mut_ptr() as *mut libc::c_void;
            msg.msg_controllen = cmsg_space.len() as _;
            let n = libc::recvmsg(stream.as_raw_fd(), &mut msg, 0);
            if n < 0 {
                return Err(std::io::Error::last_os_error()).context("recvmsg");
            }
            if msg.msg_flags & libc::MSG_CTRUNC != 0 {
                bail!("recvmsg returned MSG_CTRUNC — ancillary buffer too small");
            }
            if msg.msg_flags & libc::MSG_TRUNC != 0 {
                bail!("recvmsg returned MSG_TRUNC — data buffer too small for the JSON payload");
            }
            // Walk the control-message header to find the SCM_RIGHTS fd.
            let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
            let mut fd: Option<libc::c_int> = None;
            while !cmsg.is_null() {
                let chdr = &*cmsg;
                if chdr.cmsg_level == libc::SOL_SOCKET && chdr.cmsg_type == libc::SCM_RIGHTS {
                    let data = libc::CMSG_DATA(cmsg) as *const libc::c_int;
                    fd = Some(std::ptr::read_unaligned(data));
                    break;
                }
                cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
            }
            (n as usize, fd)
        };

        let fd = fd.context("no SCM_RIGHTS fd received from firecracker")?;
        // SAFETY: the kernel just handed us this fd; it's open and we own it.
        let uffd = unsafe { OwnedFd::from_raw_fd(fd) };

        let regions: Vec<GuestRegionUffdMapping> = serde_json::from_slice(&buf[..n_data])
            .with_context(|| {
                format!(
                    "parse firecracker region descriptor JSON ({} bytes)",
                    n_data
                )
            })?;

        Ok(Handshake { uffd, regions })
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::io::IoSlice;
        use std::os::fd::AsFd;
        use std::os::unix::net::UnixStream;

        /// Spin up a `socketpair`, send a fake handshake from one end,
        /// and verify the receiver reconstructs the regions + owns the fd.
        #[test]
        fn handshake_round_trips_over_socketpair() {
            // We use an unrelated open fd (/dev/null) as the stand-in
            // for the uffd file descriptor — the handshake parser only
            // needs *an* fd, not specifically a userfaultfd.
            let dev_null = std::fs::OpenOptions::new()
                .read(true)
                .open("/dev/null")
                .expect("open /dev/null");

            let regions = vec![GuestRegionUffdMapping {
                base_host_virt_addr: 0x7f00_1000_0000,
                size: 0x1000_0000,
                offset: 0,
                page_size_kib: 4,
            }];
            let payload = serde_json::to_vec(&regions).unwrap();

            let (a, b) = UnixStream::pair().expect("socketpair");

            // Sender side: emulate firecracker.
            let send_payload = payload.clone();
            let send_fd = dev_null.as_fd().as_raw_fd();
            let sender = std::thread::spawn(move || {
                send_handshake_test_helper(&a, &send_payload, send_fd).expect("send handshake");
            });

            let hs = recv_handshake(&b).expect("recv handshake");
            sender.join().unwrap();

            assert_eq!(hs.regions, regions);
            // The OwnedFd we received is a duped copy — should be a
            // different fd number than the sender's, but pointing at
            // /dev/null so a read of 0 bytes should succeed.
            let mut buf = [0u8; 1];
            let n = unsafe { libc::read(hs.uffd.as_raw_fd(), buf.as_mut_ptr() as *mut _, 0) };
            assert_eq!(n, 0, "received fd should be readable");
        }

        // Inline minimal sender — only used by the round-trip test, so
        // it's local rather than exposed in the library surface.
        fn send_handshake_test_helper(
            stream: &UnixStream,
            payload: &[u8],
            fd: libc::c_int,
        ) -> anyhow::Result<()> {
            let iov = [IoSlice::new(payload)];
            unsafe {
                let mut cmsg_space = [0u8; 64];
                let mut msg: libc::msghdr = std::mem::zeroed();
                msg.msg_iov = iov.as_ptr() as *mut libc::iovec;
                msg.msg_iovlen = iov.len() as _;
                msg.msg_control = cmsg_space.as_mut_ptr() as *mut libc::c_void;
                msg.msg_controllen = cmsg_space.len() as _;

                let cmsg = libc::CMSG_FIRSTHDR(&msg);
                (*cmsg).cmsg_level = libc::SOL_SOCKET;
                (*cmsg).cmsg_type = libc::SCM_RIGHTS;
                (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<libc::c_int>() as u32) as _;
                let data = libc::CMSG_DATA(cmsg) as *mut libc::c_int;
                std::ptr::write_unaligned(data, fd);
                msg.msg_controllen =
                    libc::CMSG_SPACE(std::mem::size_of::<libc::c_int>() as u32) as _;

                let n = libc::sendmsg(stream.as_raw_fd(), &msg, 0);
                if n < 0 {
                    return Err(std::io::Error::last_os_error().into());
                }
            }
            Ok(())
        }
    }
}
