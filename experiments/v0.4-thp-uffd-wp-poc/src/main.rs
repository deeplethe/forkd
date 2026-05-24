//! v0.4 Phase 3 PoC — UFFD_WP × transparent hugepages.
//!
//! Answers `DESIGN-v0.4.md` open question #2: when the source memfd is
//! backed by THPs, does `UFFDIO_WRITEPROTECT` operate at 4 KiB or 2 MiB
//! granularity, and what does it cost?
//!
//! The kernel's published behavior is that UFFD_WP is 4 KiB-granular even
//! on THP-backed regions — the WP arm or first WP fault will split each
//! hugepage into base pages. This PoC measures the cost: is the split
//! amortized at arm time (slow arm, fast faults) or at first-fault time
//! (fast arm, slow first fault)?
//!
//! Design relevance: if THP imposes a significant up-front cost on WP
//! arm, forkd will want to disable THP on source-VM memory regions to
//! keep the BRANCH pause window predictable. If the cost is small or
//! deferred to faults, we leave THP enabled (better steady-state TLB
//! behavior for the running parent).
//!
//! Methodology: run the same workload twice on a fresh memfd:
//!   - Phase A: `madvise(MADV_HUGEPAGE)` + touch every page to fault in
//!     hugepages, then arm WP, then write one byte to one page, measure.
//!   - Phase B: `madvise(MADV_NOHUGEPAGE)` + same workload.
//! Read `/proc/self/smaps` before and after WP arm to see how many bytes
//! of AnonHugePages are actually in the VMA at each point.
//!
//! Linux x86_64, kernel ≥ 5.7, run as root or
//! `sudo sysctl vm.unprivileged_userfaultfd=1`. The host needs THP
//! enabled (`cat /sys/kernel/mm/transparent_hugepage/enabled` shows
//! `[madvise]` or `[always]`).

mod uffd_raw;

use std::fs;
use std::os::fd::AsRawFd;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use nix::sys::memfd::{memfd_create, MemFdCreateFlag};

const PAGE_SIZE: usize = 4096;
const HUGEPAGE_SIZE: usize = 2 * 1024 * 1024;
const REGION_SIZE: usize = 64 * 1024 * 1024; // 64 MiB = 32 potential hugepages, 16384 base pages

fn main() -> Result<()> {
    println!("=== v0.4 Phase 3 PoC: UFFD_WP × transparent hugepages ===\n");

    print_thp_config()?;
    println!();

    // Phase A — memfd + MADV_HUGEPAGE. On most stock systems
    // (shmem_enabled=[never]) this won't actually allocate hugepages but
    // it does mark the VMA as VM_HUGEPAGE — useful to measure if the
    // marker alone has overhead.
    println!("--- Phase A: memfd + MADV_HUGEPAGE ---");
    run_one(Backing::Memfd, true)?;
    println!();

    // Phase B — memfd baseline, no THP hint.
    println!("--- Phase B: memfd + MADV_NOHUGEPAGE ---");
    run_one(Backing::Memfd, false)?;
    println!();

    // Phase C — MAP_ANONYMOUS + MADV_HUGEPAGE. Anonymous memory ignores
    // shmem_enabled and respects MADV_HUGEPAGE directly, so this is the
    // path where THPs actually get allocated. Tests the cost of real
    // hugepage split at WP arm time.
    println!("--- Phase C: MAP_ANONYMOUS + MADV_HUGEPAGE ---");
    run_one(Backing::Anonymous, true)?;
    Ok(())
}

#[derive(Clone, Copy, Debug)]
enum Backing {
    Memfd,
    Anonymous,
}

fn run_one(backing: Backing, use_thp: bool) -> Result<()> {
    let _memfd_keep_alive;
    let region = match backing {
        Backing::Memfd => {
            let memfd_name = std::ffi::CString::new("v0.4-thp-poc")?;
            let memfd =
                memfd_create(&memfd_name, MemFdCreateFlag::MFD_CLOEXEC).context("memfd_create")?;
            nix::unistd::ftruncate(&memfd, REGION_SIZE as i64).context("ftruncate")?;
            let r = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    REGION_SIZE,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    memfd.as_raw_fd(),
                    0,
                )
            };
            _memfd_keep_alive = Some(memfd);
            r
        }
        Backing::Anonymous => {
            _memfd_keep_alive = None;
            unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    REGION_SIZE,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                    -1,
                    0,
                )
            }
        }
    };
    if region == libc::MAP_FAILED {
        bail!("mmap: {}", std::io::Error::last_os_error());
    }
    let region_addr = region as usize;
    let region_ptr = region as *mut u8;

    // Advise THP behavior.
    let advice = if use_thp {
        libc::MADV_HUGEPAGE
    } else {
        libc::MADV_NOHUGEPAGE
    };
    let rc = unsafe { libc::madvise(region, REGION_SIZE, advice) };
    if rc != 0 {
        bail!("madvise: {}", std::io::Error::last_os_error());
    }

    // Touch every base page to fault in physical backing. For MADV_HUGEPAGE,
    // we touch the first byte of each 2 MiB-aligned chunk to encourage the
    // kernel to allocate a hugepage; the rest of the chunk is then
    // implicitly populated when read.
    let populate_start = Instant::now();
    for offset in (0..REGION_SIZE).step_by(PAGE_SIZE) {
        unsafe {
            *region_ptr.add(offset) = 0xAA;
        }
    }
    let populate_elapsed = populate_start.elapsed();

    // Give the kernel a moment to collapse contiguous 4K pages into THPs
    // via khugepaged when MADV_HUGEPAGE is set. khugepaged is opportunistic
    // — the result will vary by system load.
    if use_thp {
        std::thread::sleep(std::time::Duration::from_millis(500));
    }

    let hugepages_before = read_anon_hugepages_in_smaps(region_addr, REGION_SIZE)?;
    println!(
        "[populate] {} pages touched in {:?} → AnonHugePages = {} KiB ({} / {} hugepages)",
        REGION_SIZE / PAGE_SIZE,
        populate_elapsed,
        hugepages_before,
        hugepages_before / (HUGEPAGE_SIZE / 1024),
        REGION_SIZE / HUGEPAGE_SIZE
    );

    // Create uffd, register WP, arm WP. Time the arm.
    let uffd = uffd_raw::create_uffd().context("create uffd")?;
    let _ioctls = uffd_raw::register_wp(&uffd, region, REGION_SIZE).context("register WP")?;

    let arm_start = Instant::now();
    uffd_raw::writeprotect(&uffd, region, REGION_SIZE, true).context("arm WP")?;
    let arm_elapsed = arm_start.elapsed();

    let hugepages_after = read_anon_hugepages_in_smaps(region_addr, REGION_SIZE)?;
    println!(
        "[wp arm] {:?} → AnonHugePages now {} KiB ({} hugepages)",
        arm_elapsed,
        hugepages_after,
        hugepages_after / (HUGEPAGE_SIZE / 1024)
    );

    // Trigger one write fault to a specific page in the middle of the region,
    // verify the fault address is the 4 KiB page we wrote, not the surrounding
    // 2 MiB hugepage.
    let target_page_idx = (REGION_SIZE / PAGE_SIZE) / 2 + 17; // arbitrary in middle
    let target_offset = target_page_idx * PAGE_SIZE;

    // Spawn a brief handler that captures one fault and reports it.
    // We can't write from the main thread because the handler is the main
    // thread; do a quick fork-style two-thread dance.
    let uffd_arc = std::sync::Arc::new(uffd);
    let captured_addr = std::sync::Arc::new(std::sync::Mutex::new(None::<usize>));
    let handler = {
        let uffd = std::sync::Arc::clone(&uffd_arc);
        let captured_addr = std::sync::Arc::clone(&captured_addr);
        std::thread::spawn(move || -> Result<()> {
            for _ in 0..50 {
                if let Some(msg) = uffd_raw::poll_event(&uffd, 100)? {
                    if msg.event == uffd_raw::UFFD_EVENT_PAGEFAULT {
                        let (_flags, addr) = msg.as_pagefault();
                        *captured_addr.lock().unwrap() = Some(addr as usize);
                        // Clear WP so the writer can proceed.
                        let page_aligned = (addr as usize) & !(PAGE_SIZE - 1);
                        uffd_raw::writeprotect(&uffd, page_aligned as *mut _, PAGE_SIZE, false)?;
                        return Ok(());
                    }
                }
            }
            Ok(())
        })
    };

    std::thread::sleep(std::time::Duration::from_millis(50));

    let write_start = Instant::now();
    unsafe {
        *region_ptr.add(target_offset) = 0xBB;
    }
    let write_elapsed = write_start.elapsed();

    handler
        .join()
        .map_err(|_| anyhow::anyhow!("handler panicked"))??;

    let captured = *captured_addr.lock().unwrap();
    let captured_offset = captured.map(|a| a - region_addr);
    let captured_page = captured_offset.map(|o| o / PAGE_SIZE);
    let target_aligned = target_offset & !(PAGE_SIZE - 1);

    println!(
        "[first-fault] write to offset 0x{:x} (page {}) took {:?}",
        target_offset, target_page_idx, write_elapsed
    );
    println!(
        "[first-fault] handler captured addr at offset 0x{:x?} (page {:?}), expected page {} (offset 0x{:x})",
        captured_offset,
        captured_page,
        target_page_idx,
        target_aligned
    );

    // After the write, check smaps again to see if a hugepage got split.
    let hugepages_post_write = read_anon_hugepages_in_smaps(region_addr, REGION_SIZE)?;
    println!(
        "[post-write] AnonHugePages now {} KiB ({} hugepages)",
        hugepages_post_write,
        hugepages_post_write / (HUGEPAGE_SIZE / 1024)
    );

    unsafe {
        libc::munmap(region, REGION_SIZE);
    }
    Ok(())
}

fn print_thp_config() -> Result<()> {
    let enabled = fs::read_to_string("/sys/kernel/mm/transparent_hugepage/enabled")
        .unwrap_or_else(|_| "unknown".into());
    let defrag = fs::read_to_string("/sys/kernel/mm/transparent_hugepage/defrag")
        .unwrap_or_else(|_| "unknown".into());
    println!("[host] transparent_hugepage/enabled = {}", enabled.trim());
    println!("[host] transparent_hugepage/defrag  = {}", defrag.trim());
    Ok(())
}

/// Walk /proc/self/smaps and sum AnonHugePages over any VMA that
/// intersects [vma_start, vma_start+vma_len).
fn read_anon_hugepages_in_smaps(vma_start: usize, vma_len: usize) -> Result<usize> {
    let smaps = fs::read_to_string("/proc/self/smaps").context("read smaps")?;
    let mut current_vma: Option<(usize, usize)> = None;
    let mut total_kib = 0usize;
    let want_end = vma_start + vma_len;

    for line in smaps.lines() {
        // VMA header lines look like:
        //   7f8a3a000000-7f8a3e000000 rw-p 00000000 00:01 12345 /memfd:...
        if let Some(dash) = line.find('-') {
            let after_dash = &line[dash + 1..];
            if let Some(space) = after_dash.find(' ') {
                let start_str = &line[..dash];
                let end_str = &after_dash[..space];
                if let (Ok(start), Ok(end)) = (
                    usize::from_str_radix(start_str, 16),
                    usize::from_str_radix(end_str, 16),
                ) {
                    current_vma = Some((start, end));
                    continue;
                }
            }
        }
        if let Some((s, e)) = current_vma {
            // Only credit AnonHugePages from VMAs intersecting our region.
            if s < want_end && e > vma_start {
                if let Some(rest) = line.strip_prefix("AnonHugePages:") {
                    let trimmed = rest.trim();
                    let kib_part = trimmed.split_whitespace().next().unwrap_or("0");
                    total_kib += kib_part.parse::<usize>().unwrap_or(0);
                }
            }
        }
    }
    Ok(total_kib)
}
