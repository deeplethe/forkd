//! v0.4 Phase 4 PoC — end-to-end WpBranch with restore validation.
//!
//! What Phase 2 didn't cover: the snapshot file produced by WpBranch
//! is byte-consistent with the pre-WP-arm guest memory state, but is
//! it *functionally* restorable? This PoC validates that round-trip.
//!
//! Sequence:
//!
//! 1. Create a "source" memfd, place guest code at GPA 0x100, set the
//!    BEFORE marker at GPA 0x1000.
//! 2. Boot a KVM VM with that memfd as guest memory.
//! 3. Run the vcpu — guest writes 0x42 to GPA 0x1000 and halts. The
//!    source memfd now reflects post-write state.
//! 4. Reset the BEFORE marker (so the snapshot will capture BEFORE).
//! 5. Use `forkd_uffd::wp_snapshot::WpBranch` (the production library,
//!    not raw ioctls) to capture a snapshot. The guest is paused; we
//!    arm WP, bulk-copy, finalize.
//! 6. Tear down the source VM.
//! 7. Create a "destination" memfd, copy the snapshot file contents
//!    into it (this simulates FC restoring from a snapshot file).
//! 8. Boot a *fresh* KVM VM backed by the destination memfd.
//! 9. Run the vcpu — same guest code, writes 0x42 again.
//! 10. Verify: destination memfd at GPA 0x1000 holds 0x42 (the
//!     restored VM ran the same code path and produced the same
//!     write).
//!
//! If step 10 succeeds, the WpBranch-captured snapshot is functionally
//! restorable — not just bit-consistent.
//!
//! Linux x86_64, kernel >= 5.7. Run as root or
//! `sudo sysctl vm.unprivileged_userfaultfd=1`.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::Instant;

use anyhow::{bail, Context, Result};
use forkd_uffd::wp_snapshot::WpBranch;
use kvm_bindings::kvm_userspace_memory_region;
use kvm_ioctls::{Kvm, VcpuExit};

const MEMFD_SIZE: usize = 1024 * 1024; // 1 MiB
const GUEST_CODE_GPA: u64 = 0x100;
const TARGET_GPA: u64 = 0x1000;
const BEFORE_MARKER: u8 = 0xBE;
const AFTER_MARKER: u8 = 0x42;

// 16-bit real mode:
//   B0 42       mov al, 0x42
//   A2 00 10    mov [0x1000], al
//   F4          hlt
const GUEST_CODE: &[u8] = &[0xB0, 0x42, 0xA2, 0x00, 0x10, 0xF4];

fn main() -> Result<()> {
    println!("=== v0.4 Phase 4 PoC: WpBranch round-trip (snapshot → restore → re-run) ===\n");

    // -----------------------------------------------------------------
    // Stage 1: source VM
    // -----------------------------------------------------------------
    println!("[stage 1] source VM");
    let (source_memfd, source_region, _source_keepalive) =
        create_memfd_region("v0.4-phase4-source")?;
    let source_addr = source_region as usize;
    let source_ptr = source_region as *mut u8;

    // Pre-populate.
    unsafe {
        *source_ptr.add(TARGET_GPA as usize) = BEFORE_MARKER;
        std::ptr::copy_nonoverlapping(
            GUEST_CODE.as_ptr(),
            source_ptr.add(GUEST_CODE_GPA as usize),
            GUEST_CODE.len(),
        );
    }

    // Boot + run source VM.
    let kvm = Kvm::new().context("Kvm::new")?;
    let vm = kvm.create_vm().context("create_vm")?;
    let mem_region = kvm_userspace_memory_region {
        slot: 0,
        guest_phys_addr: 0,
        memory_size: MEMFD_SIZE as u64,
        userspace_addr: source_addr as u64,
        flags: 0,
    };
    unsafe { vm.set_user_memory_region(mem_region) }.context("set_user_memory_region (source)")?;
    let mut vcpu = vm.create_vcpu(0).context("create_vcpu (source)")?;

    let mut sregs = vcpu.get_sregs().context("get_sregs")?;
    sregs.cs.base = 0;
    sregs.cs.selector = 0;
    vcpu.set_sregs(&sregs).context("set_sregs")?;
    let mut regs = vcpu.get_regs().context("get_regs")?;
    regs.rip = GUEST_CODE_GPA;
    regs.rflags = 2;
    vcpu.set_regs(&regs).context("set_regs")?;

    println!("  running source vcpu...");
    loop {
        match vcpu.run().context("vcpu.run (source)")? {
            VcpuExit::Hlt => break,
            VcpuExit::IoIn(..) | VcpuExit::IoOut(..) | VcpuExit::MmioRead(..) | VcpuExit::MmioWrite(..) => {}
            other => bail!("unexpected source vcpu exit: {other:?}"),
        }
    }
    let live_after_run = unsafe { *source_ptr.add(TARGET_GPA as usize) };
    println!("  source vcpu halted; memfd[0x{TARGET_GPA:x}] = 0x{live_after_run:02x} (expected 0x{AFTER_MARKER:02x})");
    if live_after_run != AFTER_MARKER {
        bail!("source guest didn't write AFTER marker");
    }

    // The source vCPU has finished. We're about to snapshot — but our
    // snapshot test wants the BEFORE state in the snapshot, so for this
    // PoC we reset the marker before WpBranch arms. (In real v0.4, the
    // guest is *paused* mid-execution and WpBranch captures the live
    // state — no reset needed. We're testing the round-trip mechanism,
    // not the timing of the capture point.)
    unsafe {
        *source_ptr.add(TARGET_GPA as usize) = BEFORE_MARKER;
    }

    // Drop the source vcpu/vm before tearing down; the snapshot
    // operation doesn't need KVM. (We can't drop the memfd or mmap yet
    // — WpBranch needs them.)
    drop(vcpu);
    drop(vm);

    // -----------------------------------------------------------------
    // Stage 2: WpBranch — capture snapshot
    // -----------------------------------------------------------------
    println!("\n[stage 2] WpBranch capture");
    let snapshot_path = std::env::temp_dir().join("v0.4-phase4-snapshot.bin");
    let _ = std::fs::remove_file(&snapshot_path);

    let snap_start = Instant::now();
    let branch = unsafe {
        WpBranch::begin(source_memfd, source_region, MEMFD_SIZE, &snapshot_path)
            .context("WpBranch::begin")?
    };
    println!("  arm: {:?}", branch.arm_duration());
    let bulk = unsafe { branch.bulk_copy_clean() }.context("bulk_copy_clean")?;
    println!("  bulk_copy: {bulk} pages");
    let stats = branch.finalize().context("finalize")?;
    let snap_elapsed = snap_start.elapsed();
    println!(
        "  snapshot written to {} (stats={:?}, total {:?})",
        snapshot_path.display(),
        stats,
        snap_elapsed
    );

    // Verify snapshot contents.
    let snap_data = std::fs::read(&snapshot_path).context("read snapshot")?;
    if snap_data.len() != MEMFD_SIZE {
        bail!(
            "snapshot file size mismatch: {} != {MEMFD_SIZE}",
            snap_data.len()
        );
    }
    if snap_data[TARGET_GPA as usize] != BEFORE_MARKER {
        bail!(
            "snapshot[0x{TARGET_GPA:x}] = 0x{:02x}, expected 0x{BEFORE_MARKER:02x}",
            snap_data[TARGET_GPA as usize]
        );
    }
    println!("  ✓ snapshot has BEFORE marker at GPA 0x{TARGET_GPA:x}");

    // Tear down source mmap (memfd was consumed by WpBranch and dropped
    // inside finalize).
    unsafe {
        libc::munmap(source_region, MEMFD_SIZE);
    }

    // -----------------------------------------------------------------
    // Stage 3: restore into a fresh VM
    // -----------------------------------------------------------------
    println!("\n[stage 3] restore + re-run");
    let (_dest_memfd, dest_region, _dest_keepalive) = create_memfd_region("v0.4-phase4-dest")?;
    let dest_addr = dest_region as usize;
    let dest_ptr = dest_region as *mut u8;

    // Copy snapshot into the destination memfd. This simulates FC's
    // memory-from-file restore path.
    unsafe {
        std::ptr::copy_nonoverlapping(snap_data.as_ptr(), dest_ptr, MEMFD_SIZE);
    }
    let dest_before_run = unsafe { *dest_ptr.add(TARGET_GPA as usize) };
    if dest_before_run != BEFORE_MARKER {
        bail!(
            "destination memfd[0x{TARGET_GPA:x}] = 0x{dest_before_run:02x} \
             after snapshot load, expected 0x{BEFORE_MARKER:02x}"
        );
    }
    println!("  ✓ destination memfd loaded with snapshot (BEFORE marker confirmed)");

    let vm2 = kvm.create_vm().context("create_vm (dest)")?;
    let dest_region_kvm = kvm_userspace_memory_region {
        slot: 0,
        guest_phys_addr: 0,
        memory_size: MEMFD_SIZE as u64,
        userspace_addr: dest_addr as u64,
        flags: 0,
    };
    unsafe { vm2.set_user_memory_region(dest_region_kvm) }
        .context("set_user_memory_region (dest)")?;
    let mut vcpu2 = vm2.create_vcpu(0).context("create_vcpu (dest)")?;

    let mut sregs2 = vcpu2.get_sregs().context("get_sregs (dest)")?;
    sregs2.cs.base = 0;
    sregs2.cs.selector = 0;
    vcpu2.set_sregs(&sregs2).context("set_sregs (dest)")?;
    let mut regs2 = vcpu2.get_regs().context("get_regs (dest)")?;
    regs2.rip = GUEST_CODE_GPA;
    regs2.rflags = 2;
    vcpu2.set_regs(&regs2).context("set_regs (dest)")?;

    println!("  running dest vcpu (restored state)...");
    loop {
        match vcpu2.run().context("vcpu.run (dest)")? {
            VcpuExit::Hlt => break,
            VcpuExit::IoIn(..) | VcpuExit::IoOut(..) | VcpuExit::MmioRead(..) | VcpuExit::MmioWrite(..) => {}
            other => bail!("unexpected dest vcpu exit: {other:?}"),
        }
    }
    let dest_after_run = unsafe { *dest_ptr.add(TARGET_GPA as usize) };
    println!("  dest vcpu halted; memfd[0x{TARGET_GPA:x}] = 0x{dest_after_run:02x} (expected 0x{AFTER_MARKER:02x})");

    if dest_after_run != AFTER_MARKER {
        bail!(
            "restored VM didn't produce expected guest write: got 0x{dest_after_run:02x}"
        );
    }

    println!("\n=== Phase 4 PASSED ===");
    println!("  WpBranch-captured snapshot round-trip:");
    println!("    source guest writes AFTER (0x42) → memfd reflects it");
    println!("    WpBranch captures BEFORE (0xBE) → snapshot file holds it");
    println!("    fresh VM loads snapshot → re-runs guest code → writes AFTER (0x42)");
    println!("  Restore is functionally valid, not just bit-consistent.");

    unsafe {
        libc::munmap(dest_region, MEMFD_SIZE);
    }
    let _ = std::fs::remove_file(&snapshot_path);
    Ok(())
}

/// Allocate a memfd + mmap it. Returns (memfd, region_ptr, ()).
/// The third element is currently unused; reserved for future
/// keepalive of secondary resources.
fn create_memfd_region(
    name: &str,
) -> Result<(OwnedFd, *mut libc::c_void, ())> {
    const SYS_MEMFD_CREATE: libc::c_long = 319;
    const MFD_CLOEXEC: libc::c_uint = 0x0001;
    let memfd_name = std::ffi::CString::new(name)?;
    let fd = unsafe {
        libc::syscall(
            SYS_MEMFD_CREATE,
            memfd_name.as_ptr(),
            MFD_CLOEXEC as libc::c_ulong,
        )
    };
    if fd < 0 {
        bail!("memfd_create({name}): {}", std::io::Error::last_os_error());
    }
    let memfd = unsafe { OwnedFd::from_raw_fd(fd as libc::c_int) };
    if unsafe { libc::ftruncate(memfd.as_raw_fd(), MEMFD_SIZE as libc::off_t) } != 0 {
        bail!("ftruncate({name}): {}", std::io::Error::last_os_error());
    }
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
        bail!("mmap({name}): {}", std::io::Error::last_os_error());
    }
    Ok((memfd, region, ()))
}
