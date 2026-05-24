//! `forkd wp-bench` — exercise the v0.4 snapshot-side UFFD_WP path
//! against a synthetic memfd, outside the Firecracker integration.
//!
//! This is the CLI surface for the [`forkd_uffd::wp_snapshot::WpBranch`]
//! library API. It creates a memfd of the requested size, populates it
//! with a known pattern, arms WP, runs the bulk-copy + handler pair,
//! finalizes, and prints timing data in the same shape as `forkd bench`.
//!
//! The full BRANCH integration (replacing the current
//! `forkd-controller::branch_sandbox` FC-snapshot path with WpBranch +
//! a vmstate-only dump) is tracked separately in `DESIGN-v0.4.md`;
//! this subcommand exists so the WP machinery can be benchmarked and
//! demoed without that integration in place.

#[cfg(not(target_os = "linux"))]
pub fn run(_region_mib: u64, _snapshot_path: std::path::PathBuf) -> anyhow::Result<()> {
    anyhow::bail!("forkd wp-bench is Linux-only (depends on userfaultfd UFFD_WP, kernel >= 5.7)")
}

#[cfg(target_os = "linux")]
pub fn run(region_mib: u64, snapshot_path: std::path::PathBuf) -> anyhow::Result<()> {
    use anyhow::{bail, Context};
    use forkd_uffd::wp_snapshot::{WpBranch, PAGE_SIZE};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::time::Instant;

    if region_mib == 0 {
        bail!("--region-mib must be >= 1");
    }
    let region_size = (region_mib as usize) * 1024 * 1024;
    if !region_size.is_multiple_of(PAGE_SIZE) {
        bail!("region_size {region_size} is not a multiple of {PAGE_SIZE}");
    }
    let num_pages = region_size / PAGE_SIZE;

    println!("forkd wp-bench v0.4");
    println!("  region: {region_mib} MiB ({num_pages} pages of {PAGE_SIZE} bytes)");
    println!("  snapshot output: {}", snapshot_path.display());

    // 1. memfd_create — uses memfd_create(2) directly via libc so we
    //    don't pull in nix as a dep.
    let memfd_name = std::ffi::CString::new("forkd-wp-bench")?;
    // memfd_create syscall number on x86_64 is 319.
    const SYS_MEMFD_CREATE: libc::c_long = 319;
    const MFD_CLOEXEC: libc::c_uint = 0x0001;
    let fd = unsafe {
        libc::syscall(
            SYS_MEMFD_CREATE,
            memfd_name.as_ptr(),
            MFD_CLOEXEC as libc::c_ulong,
        )
    };
    if fd < 0 {
        bail!("memfd_create: {}", std::io::Error::last_os_error());
    }
    let memfd = unsafe { OwnedFd::from_raw_fd(fd as libc::c_int) };
    if unsafe { libc::ftruncate(memfd.as_raw_fd(), region_size as libc::off_t) } != 0 {
        bail!("ftruncate: {}", std::io::Error::last_os_error());
    }

    // 2. mmap the memfd. We'll keep this address through the WpBranch
    //    lifetime (the unsafe contract on begin()).
    let region = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            region_size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            memfd.as_raw_fd(),
            0,
        )
    };
    if region == libc::MAP_FAILED {
        bail!("mmap: {}", std::io::Error::last_os_error());
    }
    let region_ptr = region as *mut u8;

    // 3. Pre-populate with a deterministic pattern so the bulk-copy
    //    has something interesting to copy (and so verifying the
    //    snapshot later is meaningful).
    let populate_start = Instant::now();
    unsafe {
        std::ptr::write_bytes(region_ptr, 0x42, region_size);
    }
    let populate_elapsed = populate_start.elapsed();
    println!("  populated in {populate_elapsed:?}");

    // 4. Begin a WpBranch — this arms UFFDIO_WRITEPROTECT and spawns
    //    the handler thread. The arm latency is what would be the
    //    BRANCH pause-window analog in the full integration.
    let branch_start = Instant::now();
    let branch = unsafe {
        WpBranch::begin(memfd, region, region_size, &snapshot_path).context("WpBranch::begin")?
    };
    let arm = branch.arm_duration();
    println!("  arm UFFDIO_WRITEPROTECT: {arm:?}");

    // 5. Bulk-copy the still-clean pages. Since we never wrote after
    //    arming, every page should be bulk-copied (no faults caught).
    let bulk_start = Instant::now();
    let bulk_copied = unsafe { branch.bulk_copy_clean() }.context("bulk_copy_clean")?;
    let bulk_elapsed = bulk_start.elapsed();
    println!("  bulk_copy_clean: {bulk_copied} pages in {bulk_elapsed:?}");

    // 6. Finalize — stops handler, fsyncs snapshot, returns stats.
    let finalize_start = Instant::now();
    let stats = branch.finalize().context("finalize")?;
    let finalize_elapsed = finalize_start.elapsed();
    let total_elapsed = branch_start.elapsed();

    println!("  finalize: {finalize_elapsed:?}");
    println!();
    println!("  arm_duration             {:?}", stats.arm_duration);
    println!(
        "  pages_captured_by_fault  {}",
        stats.pages_captured_by_fault
    );
    println!(
        "  pages_captured_by_bulk   {}",
        stats.pages_captured_by_bulk
    );
    println!("  total_pages              {}", stats.total_pages);
    println!();
    println!("  total                    {total_elapsed:?}");

    // Verify snapshot content.
    let snap_data = std::fs::read(&snapshot_path).context("read snapshot for verify")?;
    if snap_data.len() != region_size {
        bail!(
            "snapshot file size {} != region size {region_size}",
            snap_data.len()
        );
    }
    let bad = snap_data.iter().filter(|&&b| b != 0x42).count();
    if bad > 0 {
        bail!("{bad} bytes in snapshot don't match the pre-arm pattern (0x42)");
    }
    println!();
    println!(
        "  ✓ snapshot consistent ({} bytes all 0x42)",
        snap_data.len()
    );

    // Cleanup the mmap.
    unsafe {
        libc::munmap(region, region_size);
    }
    Ok(())
}
