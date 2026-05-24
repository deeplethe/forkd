//! Raw libc wrappers for the userfaultfd ioctls we need.
//!
//! The `userfaultfd` crate (0.8.1) doesn't yet expose `UFFDIO_WRITEPROTECT`
//! or the WP register mode, so we issue the ioctls directly. ioctl numbers
//! are computed per `<linux/userfaultfd.h>` and `<asm-generic/ioctl.h>`:
//!
//!     #define _IOC(dir, type, nr, size)   (((dir)<<30)|((size)<<16)|((type)<<8)|(nr))
//!     #define _IOWR(type, nr, size)       _IOC(3, type, nr, sizeof(size))
//!     #define UFFDIO                      0xAA

#![allow(dead_code)]

use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};

use anyhow::{bail, Context, Result};

// --- ioctl numbers (x86_64, computed by hand from <linux/userfaultfd.h>) ---

const _IOC_WRITE: u32 = 1;
const _IOC_READ: u32 = 2;
const _IOC_NRBITS: u32 = 8;
const _IOC_TYPEBITS: u32 = 8;
const _IOC_SIZEBITS: u32 = 14;
const _IOC_DIRBITS: u32 = 2;

const _IOC_NRSHIFT: u32 = 0;
const _IOC_TYPESHIFT: u32 = _IOC_NRSHIFT + _IOC_NRBITS;
const _IOC_SIZESHIFT: u32 = _IOC_TYPESHIFT + _IOC_TYPEBITS;
const _IOC_DIRSHIFT: u32 = _IOC_SIZESHIFT + _IOC_SIZEBITS;

const fn ioc(dir: u32, ty: u32, nr: u32, size: u32) -> libc::c_ulong {
    ((dir << _IOC_DIRSHIFT)
        | (ty << _IOC_TYPESHIFT)
        | (nr << _IOC_NRSHIFT)
        | (size << _IOC_SIZESHIFT)) as libc::c_ulong
}

const fn iowr<T>(ty: u32, nr: u32) -> libc::c_ulong {
    ioc(
        _IOC_READ | _IOC_WRITE,
        ty,
        nr,
        std::mem::size_of::<T>() as u32,
    )
}

const UFFDIO: u32 = 0xAA;
const UFFDIO_API_NR: u32 = 0x3F;
const UFFDIO_REGISTER_NR: u32 = 0x00;
const UFFDIO_WRITEPROTECT_NR: u32 = 0x06;
const UFFDIO_WAKE_NR: u32 = 0x02;

// --- kernel structs (mirror <linux/userfaultfd.h>) ---

#[repr(C)]
#[derive(Default)]
pub struct UffdioApi {
    pub api: u64,
    pub features: u64,
    pub ioctls: u64,
}

#[repr(C)]
#[derive(Default)]
pub struct UffdioRange {
    pub start: u64,
    pub len: u64,
}

#[repr(C)]
#[derive(Default)]
pub struct UffdioRegister {
    pub range: UffdioRange,
    pub mode: u64,
    pub ioctls: u64,
}

#[repr(C)]
#[derive(Default)]
pub struct UffdioWriteprotect {
    pub range: UffdioRange,
    pub mode: u64,
}

// --- bit constants ---

pub const UFFD_API: u64 = 0xAA;
pub const UFFD_FEATURE_PAGEFAULT_FLAG_WP: u64 = 1 << 9;

pub const UFFDIO_REGISTER_MODE_MISSING: u64 = 1 << 0;
pub const UFFDIO_REGISTER_MODE_WP: u64 = 1 << 1;

pub const UFFDIO_WRITEPROTECT_MODE_WP: u64 = 1 << 0;
pub const UFFDIO_WRITEPROTECT_MODE_DONTWAKE: u64 = 1 << 1;

// --- uffd_msg layout (for read events) ---

pub const UFFD_EVENT_PAGEFAULT: u8 = 0x12;
pub const UFFD_PAGEFAULT_FLAG_WRITE: u64 = 1 << 0;
pub const UFFD_PAGEFAULT_FLAG_WP: u64 = 1 << 1;

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct UffdMsg {
    pub event: u8,
    pub reserved1: u8,
    pub reserved2: u16,
    pub reserved3: u32,
    // Union — pagefault is the largest variant. We treat it as raw bytes,
    // then read the pagefault fields when event == UFFD_EVENT_PAGEFAULT.
    pub arg: [u8; 24],
}

impl UffdMsg {
    pub fn as_pagefault(&self) -> (u64, u64) {
        // struct { __u64 flags; __u64 address; ... } at offset 0 of arg.
        let flags = u64::from_ne_bytes(self.arg[0..8].try_into().unwrap());
        let address = u64::from_ne_bytes(self.arg[8..16].try_into().unwrap());
        (flags, address)
    }
}

// --- high-level wrappers ---

/// Create a userfaultfd, negotiate UFFD_API with WP feature.
pub fn create_uffd() -> Result<OwnedFd> {
    // userfaultfd(2) syscall: SYS_userfaultfd = 323 on x86_64.
    const SYS_USERFAULTFD: libc::c_long = 323;
    const O_CLOEXEC: libc::c_int = 0o2000000;
    const O_NONBLOCK: libc::c_int = 0o4000;

    let fd = unsafe { libc::syscall(SYS_USERFAULTFD, O_CLOEXEC | O_NONBLOCK) };
    if fd < 0 {
        bail!(
            "userfaultfd(2): {} \
             (try `sudo sysctl vm.unprivileged_userfaultfd=1` or run as root)",
            io::Error::last_os_error()
        );
    }
    let owned = unsafe { OwnedFd::from_raw_fd(fd as RawFd) };

    // UFFDIO_API: declare API + request WP feature.
    let mut api = UffdioApi {
        api: UFFD_API,
        features: UFFD_FEATURE_PAGEFAULT_FLAG_WP,
        ioctls: 0,
    };
    let rc = unsafe {
        libc::ioctl(
            owned.as_raw_fd(),
            iowr::<UffdioApi>(UFFDIO, UFFDIO_API_NR),
            &mut api as *mut _,
        )
    };
    if rc != 0 {
        bail!("UFFDIO_API: {}", io::Error::last_os_error());
    }
    if (api.features & UFFD_FEATURE_PAGEFAULT_FLAG_WP) == 0 {
        bail!(
            "kernel does not advertise UFFD_FEATURE_PAGEFAULT_FLAG_WP \
             (negotiated features: 0x{:x}). Need kernel >= 5.7.",
            api.features
        );
    }
    Ok(owned)
}

/// Register a memory region with WRITE_PROTECT mode.
pub fn register_wp(uffd: &OwnedFd, addr: *mut libc::c_void, len: usize) -> Result<u64> {
    let mut reg = UffdioRegister {
        range: UffdioRange {
            start: addr as u64,
            len: len as u64,
        },
        mode: UFFDIO_REGISTER_MODE_WP,
        ioctls: 0,
    };
    let rc = unsafe {
        libc::ioctl(
            uffd.as_raw_fd(),
            iowr::<UffdioRegister>(UFFDIO, UFFDIO_REGISTER_NR),
            &mut reg as *mut _,
        )
    };
    if rc != 0 {
        bail!("UFFDIO_REGISTER (WP): {}", io::Error::last_os_error());
    }
    Ok(reg.ioctls)
}

/// Arm or clear write-protection on a range.
pub fn writeprotect(uffd: &OwnedFd, addr: *mut libc::c_void, len: usize, wp: bool) -> Result<()> {
    let mut wp_arg = UffdioWriteprotect {
        range: UffdioRange {
            start: addr as u64,
            len: len as u64,
        },
        mode: if wp { UFFDIO_WRITEPROTECT_MODE_WP } else { 0 },
    };
    let rc = unsafe {
        libc::ioctl(
            uffd.as_raw_fd(),
            iowr::<UffdioWriteprotect>(UFFDIO, UFFDIO_WRITEPROTECT_NR),
            &mut wp_arg as *mut _,
        )
    };
    if rc != 0 {
        bail!(
            "UFFDIO_WRITEPROTECT (wp={wp}): {}",
            io::Error::last_os_error()
        );
    }
    Ok(())
}

/// Poll the uffd fd with a timeout, then read one event if available.
pub fn poll_event(uffd: &OwnedFd, timeout_ms: i32) -> Result<Option<UffdMsg>> {
    let mut pfd = libc::pollfd {
        fd: uffd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let rc = unsafe { libc::poll(&mut pfd as *mut _, 1, timeout_ms) };
    if rc < 0 {
        bail!("poll: {}", io::Error::last_os_error());
    }
    if rc == 0 || (pfd.revents & libc::POLLIN) == 0 {
        return Ok(None);
    }
    let mut msg: UffdMsg = Default::default();
    let n = unsafe {
        libc::read(
            uffd.as_raw_fd(),
            &mut msg as *mut _ as *mut libc::c_void,
            std::mem::size_of::<UffdMsg>(),
        )
    };
    if n < 0 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EAGAIN) {
            return Ok(None);
        }
        return Err(err).context("uffd read");
    }
    if (n as usize) != std::mem::size_of::<UffdMsg>() {
        bail!("uffd short read: {n} bytes");
    }
    Ok(Some(msg))
}

// std::os::fd::OwnedFd doesn't have a from_raw_fd in stable for a few releases;
// use the trait explicitly.
use std::os::fd::FromRawFd;
