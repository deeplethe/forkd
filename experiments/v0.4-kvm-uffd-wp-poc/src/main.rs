//! v0.4 Phase 2 PoC — does UFFD_WP catch KVM guest writes through EPT?
//!
//! Answers open question #1 in `DESIGN-v0.4.md` (at the repo root): when a guest accesses
//! memory via `KVM_RUN` (and therefore through EPT/NPT, not the host MMU),
//! does write-protection armed on the host VMA still produce a userspace
//! fault?
//!
//! The setup:
//!
//! 1. Create a 1 MiB memfd, mmap it, pre-populate offset 0x1000 with byte
//!    0xBE ("BEFORE marker").
//! 2. Place a tiny 16-bit real-mode guest at offset 0x100:
//!
//!        mov al, 0x42        ; B0 42
//!        mov [0x1000], al    ; A2 00 10
//!        hlt                 ; F4
//!
//! 3. Hand the memfd to KVM as a memslot.
//! 4. Arm `UFFDIO_WRITEPROTECT` over the whole memfd region.
//! 5. Spawn a uffd handler thread.
//! 6. Run the vcpu. The guest executes the `mov [0x1000], al`.
//! 7. Validate:
//!    - Handler caught a write fault at offset 0x1000.
//!    - The "snapshot" copy (handler-captured) at 0x1000 holds 0xBE.
//!    - Live memfd at 0x1000 holds 0x42 (post-write).
//!
//! If all three hold, EPT-mediated guest writes propagate through MMU
//! notifiers to UFFD_WP on the host VMA, and v0.4's snapshot mechanism is
//! sound under KVM.
//!
//! Linux x86_64, kernel ≥ 5.7 with `vm.unprivileged_userfaultfd=1`, or
//! run as root.

mod uffd_raw;

use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use kvm_bindings::kvm_userspace_memory_region;
use kvm_ioctls::{Kvm, VcpuExit};
use nix::sys::memfd::{memfd_create, MemFdCreateFlag};
use parking_lot::Mutex;

const MEMFD_SIZE: usize = 1024 * 1024; // 1 MiB
const PAGE_SIZE: usize = 4096;
const GUEST_CODE_GPA: u64 = 0x100;
const TARGET_GPA: u64 = 0x1000;
const BEFORE_MARKER: u8 = 0xBE;
const AFTER_MARKER: u8 = 0x42;

// 16-bit real-mode guest:
//   B0 42       mov al, 0x42
//   A2 00 10    mov [0x1000], al
//   F4          hlt
const GUEST_CODE: &[u8] = &[0xB0, 0x42, 0xA2, 0x00, 0x10, 0xF4];

fn main() -> Result<()> {
    println!("=== v0.4 Phase 2 PoC: UFFD_WP × KVM guest writes ===\n");

    // 1. memfd + mmap.
    let memfd_name = std::ffi::CString::new("v0.4-kvm-poc")?;
    let memfd = memfd_create(&memfd_name, MemFdCreateFlag::MFD_CLOEXEC).context("memfd_create")?;
    nix::unistd::ftruncate(&memfd, MEMFD_SIZE as i64).context("ftruncate")?;
    let region = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            MEMFD_SIZE,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            memfd.as_raw_fd(),
            0,
        )
    };
    if region == libc::MAP_FAILED {
        bail!("mmap: {}", std::io::Error::last_os_error());
    }
    let region_addr = region as usize;
    let region_ptr = region as *mut u8;
    println!(
        "[setup] memfd mmap'd at 0x{region_addr:x}, size {} KiB",
        MEMFD_SIZE / 1024
    );

    // 2. Pre-populate target page with BEFORE marker.
    unsafe {
        *region_ptr.add(TARGET_GPA as usize) = BEFORE_MARKER;
    }
    println!("[setup] wrote BEFORE marker 0x{BEFORE_MARKER:02x} to GPA 0x{TARGET_GPA:x}");

    // 3. Place guest code.
    unsafe {
        std::ptr::copy_nonoverlapping(
            GUEST_CODE.as_ptr(),
            region_ptr.add(GUEST_CODE_GPA as usize),
            GUEST_CODE.len(),
        );
    }
    println!(
        "[setup] placed {}-byte guest code at GPA 0x{GUEST_CODE_GPA:x}",
        GUEST_CODE.len()
    );

    // 4. KVM setup.
    let kvm = Kvm::new().context("Kvm::new — is /dev/kvm accessible?")?;
    let vm = kvm.create_vm().context("create_vm")?;
    let mem_region = kvm_userspace_memory_region {
        slot: 0,
        guest_phys_addr: 0,
        memory_size: MEMFD_SIZE as u64,
        userspace_addr: region_addr as u64,
        flags: 0,
    };
    unsafe { vm.set_user_memory_region(mem_region) }.context("set_user_memory_region")?;
    let mut vcpu = vm.create_vcpu(0).context("create_vcpu")?;

    // 5. Set up vcpu registers for real mode at CS:IP = 0:0x100.
    let mut sregs = vcpu.get_sregs().context("get_sregs")?;
    sregs.cs.base = 0;
    sregs.cs.selector = 0;
    vcpu.set_sregs(&sregs).context("set_sregs")?;

    let mut regs = vcpu.get_regs().context("get_regs")?;
    regs.rip = GUEST_CODE_GPA;
    regs.rflags = 2;
    vcpu.set_regs(&regs).context("set_regs")?;
    println!("[kvm] vcpu set to CS:IP = 0:0x{GUEST_CODE_GPA:x}");

    // 6. Create uffd, register region with WP mode, arm WP.
    let uffd = Arc::new(uffd_raw::create_uffd().context("create uffd")?);
    let ioctls = uffd_raw::register_wp(&uffd, region, MEMFD_SIZE).context("register WP")?;
    println!("[uffd] registered WP mode, ioctls bitmap: 0x{ioctls:x}");

    let wp_arm_start = Instant::now();
    uffd_raw::writeprotect(&uffd, region, MEMFD_SIZE, true).context("arm WP")?;
    let wp_arm_elapsed = wp_arm_start.elapsed();
    println!("[uffd] armed UFFDIO_WRITEPROTECT in {:?}", wp_arm_elapsed);

    // 7. Handler thread.
    let captured: Arc<Mutex<Vec<(usize, u64)>>> = Arc::new(Mutex::new(Vec::new()));
    let snapshot = Arc::new(Mutex::new(vec![0u8; MEMFD_SIZE]));
    let stop_handler = Arc::new(AtomicBool::new(false));
    let handler = {
        let uffd = Arc::clone(&uffd);
        let captured = Arc::clone(&captured);
        let snapshot = Arc::clone(&snapshot);
        let stop_handler = Arc::clone(&stop_handler);
        thread::spawn(move || -> Result<()> {
            while !stop_handler.load(Ordering::Acquire) {
                let msg = match uffd_raw::poll_event(&uffd, 50)? {
                    Some(m) => m,
                    None => continue,
                };
                if msg.event != uffd_raw::UFFD_EVENT_PAGEFAULT {
                    continue;
                }
                let (flags, addr) = msg.as_pagefault();
                let page_addr = (addr as usize) & !(PAGE_SIZE - 1);
                let page_offset = page_addr - region_addr;
                // Copy the page into the snapshot (still WP'd, so its
                // contents are pre-write).
                let page_slice =
                    unsafe { std::slice::from_raw_parts(page_addr as *const u8, PAGE_SIZE) };
                {
                    let mut snap = snapshot.lock();
                    snap[page_offset..page_offset + PAGE_SIZE].copy_from_slice(page_slice);
                }
                captured.lock().push((page_offset, flags));
                // Clear WP for this page so the faulting access can proceed.
                uffd_raw::writeprotect(&uffd, page_addr as *mut _, PAGE_SIZE, false)
                    .map_err(|e| anyhow!("clear WP: {e}"))?;
                println!(
                    "[handler] caught fault at GPA 0x{page_offset:x} (flags=0x{flags:x}, \
                     write={})",
                    (flags & uffd_raw::UFFD_PAGEFAULT_FLAG_WRITE) != 0
                );
            }
            Ok(())
        })
    };

    // 8. Run the vcpu.
    println!("\n[kvm] running vcpu...");
    let vcpu_run_start = Instant::now();
    let mut exit_count = 0usize;
    loop {
        exit_count += 1;
        if exit_count > 20 {
            bail!("vcpu exit loop runaway");
        }
        let exit = vcpu.run().context("vcpu.run")?;
        match exit {
            VcpuExit::Hlt => {
                println!(
                    "[kvm] guest halted normally in {:?} ({} exits)",
                    vcpu_run_start.elapsed(),
                    exit_count
                );
                break;
            }
            VcpuExit::IoIn(port, _) | VcpuExit::IoOut(port, _) => {
                println!("[kvm] guest I/O on port 0x{port:x} (ignored)");
            }
            VcpuExit::MmioRead(addr, _) | VcpuExit::MmioWrite(addr, _) => {
                println!("[kvm] guest MMIO at 0x{addr:x} (ignored)");
            }
            other => {
                println!("[kvm] vcpu exit: {other:?}");
                break;
            }
        }
    }

    // 9. Stop handler, validate.
    thread::sleep(Duration::from_millis(100));
    stop_handler.store(true, Ordering::Release);
    handler.join().map_err(|_| anyhow!("handler panicked"))??;

    let captured = captured.lock();
    let snapshot = snapshot.lock();
    let live_target_byte = unsafe { *region_ptr.add(TARGET_GPA as usize) };
    let snap_target_byte = snapshot[TARGET_GPA as usize];

    println!("\n=== Result ===");
    println!("WP arm latency:        {:?}", wp_arm_elapsed);
    println!("uffd faults caught:    {} ({:?})", captured.len(), captured);
    println!(
        "Live memfd[0x{TARGET_GPA:x}]:  0x{live_target_byte:02x} (expected 0x{AFTER_MARKER:02x} = AFTER)"
    );
    println!(
        "Snapshot[0x{TARGET_GPA:x}]:    0x{snap_target_byte:02x} (expected 0x{BEFORE_MARKER:02x} = BEFORE)"
    );

    // The headline checks.
    if live_target_byte != AFTER_MARKER {
        bail!(
            "guest never executed the write — live byte is 0x{live_target_byte:02x}, \
             expected 0x{AFTER_MARKER:02x}"
        );
    }
    let target_page = (TARGET_GPA as usize) / PAGE_SIZE * PAGE_SIZE;
    let caught_target = captured.iter().any(|(off, _)| *off == target_page);
    if !caught_target {
        bail!(
            "UFFD_WP did NOT catch the guest write to GPA 0x{TARGET_GPA:x} \
             (page 0x{target_page:x}). EPT bypass — answer to open question #1 is \
             NEGATIVE: snapshot-time WP on host VMA does not propagate to KVM guests."
        );
    }
    if snap_target_byte != BEFORE_MARKER {
        bail!(
            "snapshot byte at 0x{TARGET_GPA:x} is 0x{snap_target_byte:02x}, \
             expected 0x{BEFORE_MARKER:02x}. Handler captured the page AFTER \
             the guest write — ordering invariant broken."
        );
    }

    println!(
        "\nPoC PASSED — open question #1 answered: yes, UFFD_WP catches KVM \
         guest writes through EPT, and the pre-write content is captured \
         before the guest write commits."
    );

    unsafe {
        libc::munmap(region, MEMFD_SIZE);
    }
    Ok(())
}
