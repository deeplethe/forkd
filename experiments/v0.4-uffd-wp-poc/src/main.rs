//! v0.4 Phase 1 PoC: prove that `UFFDIO_WRITEPROTECT` on a memfd-backed VMA
//! delivers write-faults to a userspace handler, and that the handler can
//! capture the *pre-write* page content before the writer continues.
//!
//! What this exercises (and what v0.4 needs to be sound):
//!
//! 1. `memfd_create` + `mmap(MAP_SHARED)` — anonymous memory the kernel
//!    will let us write-protect via userfaultfd.
//! 2. `userfaultfd(2)` + `UFFDIO_API` negotiating `PAGEFAULT_FLAG_WP`.
//! 3. `UFFDIO_REGISTER` with `MODE_WP` over the full region.
//! 4. `UFFDIO_WRITEPROTECT` to arm WP — we time this; it is the v0.4
//!    "pause window" analog.
//! 5. A writer thread that scribbles random pages.
//! 6. A handler thread that polls the uffd, copies each first-write page
//!    into a snapshot file at the right offset, then `WRITEPROTECT mode=0`
//!    that single page so the writer can proceed.
//! 7. After the writer stops, a "bulk copier" pass to flush the still-clean
//!    pages to the snapshot file (still WP'd, safe to read directly).
//! 8. Validation: every page in the snapshot **must** start with its
//!    BEFORE label. Any AFTER content means the WP ordering invariant
//!    is broken and v0.4's correctness argument is wrong.
//!
//! Linux x86_64, kernel ≥ 5.7. Either run as root or
//! `sudo sysctl vm.unprivileged_userfaultfd=1`.

mod uffd_raw;

use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use nix::sys::memfd::{memfd_create, MemFdCreateFlag};
use parking_lot::Mutex;
use rand::Rng;

use uffd_raw::{UFFD_PAGEFAULT_FLAG_WRITE, UFFD_EVENT_PAGEFAULT};

const PAGE_SIZE: usize = 4096;
const DEFAULT_REGION_MIB: usize = 64;
const WRITER_DURATION: Duration = Duration::from_secs(3);
const SNAPSHOT_FILE: &str = "/tmp/v0.4-uffd-wp-poc.snapshot";

fn parse_region_size() -> usize {
    std::env::var("REGION_MIB")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(DEFAULT_REGION_MIB)
        * 1024
        * 1024
}

fn before_label(page_idx: usize) -> String {
    format!("PAGE_{page_idx:06}_BEFORE")
}

fn after_label(page_idx: usize) -> String {
    format!("PAGE_{page_idx:06}_AFTER ")
}

fn main() -> Result<()> {
    let region_size = parse_region_size();
    let num_pages = region_size / PAGE_SIZE;

    println!("=== v0.4 Phase 1 PoC: UFFDIO_WRITEPROTECT on memfd ===");
    println!(
        "Region: {} MiB ({} pages of {} bytes)\n",
        region_size / 1024 / 1024,
        num_pages,
        PAGE_SIZE
    );

    // 1. memfd + mmap.
    let memfd_name = std::ffi::CString::new("v0.4-poc")?;
    let memfd = memfd_create(&memfd_name, MemFdCreateFlag::MFD_CLOEXEC).context("memfd_create")?;
    nix::unistd::ftruncate(&memfd, region_size as i64).context("ftruncate")?;

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
    let region_addr = region as usize;
    println!("[setup] memfd mmap'd at 0x{region_addr:x}");

    // 2. Populate with BEFORE patterns.
    let populate_start = Instant::now();
    for page_idx in 0..num_pages {
        let label = before_label(page_idx);
        let label_bytes = label.as_bytes();
        let dest = (region_addr + page_idx * PAGE_SIZE) as *mut u8;
        unsafe {
            std::ptr::copy_nonoverlapping(label_bytes.as_ptr(), dest, label_bytes.len());
        }
    }
    println!(
        "[setup] populated {} pages with BEFORE patterns in {:?}",
        num_pages,
        populate_start.elapsed()
    );

    // 3. Create uffd, register region with WP mode.
    let uffd = Arc::new(uffd_raw::create_uffd().context("create uffd")?);
    println!("[uffd] created (fd={})", uffd.as_raw_fd());

    let ioctls = uffd_raw::register_wp(&uffd, region, region_size).context("register WP")?;
    println!("[uffd] registered WP mode, supported ioctls bitmap: 0x{ioctls:x}");

    // 4. Arm WP — the v0.4 pause-window analog.
    let wp_arm_start = Instant::now();
    uffd_raw::writeprotect(&uffd, region, region_size, true).context("arm WP")?;
    let wp_arm_elapsed = wp_arm_start.elapsed();
    println!(
        "[wp] armed UFFDIO_WRITEPROTECT over {} MiB in {:?}  ← v0.4 pause-window analog",
        region_size / 1024 / 1024,
        wp_arm_elapsed
    );

    // 5. Snapshot file.
    let snapshot = Arc::new(Mutex::new(
        OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(SNAPSHOT_FILE)?,
    ));
    snapshot.lock().set_len(region_size as u64)?;

    // 6. Shared state.
    let captured: Arc<Vec<AtomicBool>> =
        Arc::new((0..num_pages).map(|_| AtomicBool::new(false)).collect());
    let dirty_faults = Arc::new(AtomicU64::new(0));
    let writes_done = Arc::new(AtomicU64::new(0));
    let stop_handler = Arc::new(AtomicBool::new(false));

    // 7. Handler thread.
    let handler = {
        let uffd = Arc::clone(&uffd);
        let captured = Arc::clone(&captured);
        let dirty_faults = Arc::clone(&dirty_faults);
        let snapshot = Arc::clone(&snapshot);
        let stop_handler = Arc::clone(&stop_handler);
        thread::spawn(move || -> Result<()> {
            while !stop_handler.load(Ordering::Acquire) {
                let msg = match uffd_raw::poll_event(&uffd, 50)? {
                    Some(m) => m,
                    None => continue,
                };
                if msg.event != UFFD_EVENT_PAGEFAULT {
                    continue;
                }
                let (flags, addr) = msg.as_pagefault();
                if (flags & UFFD_PAGEFAULT_FLAG_WRITE) == 0 {
                    // Not a write fault; shouldn't happen with WP-only registration.
                    continue;
                }
                let page_addr = (addr as usize) & !(PAGE_SIZE - 1);
                let page_idx = (page_addr - region_addr) / PAGE_SIZE;
                if !captured[page_idx].swap(true, Ordering::AcqRel) {
                    // First time: copy page content (still WP'd, safe).
                    let page_slice =
                        unsafe { std::slice::from_raw_parts(page_addr as *const u8, PAGE_SIZE) };
                    let mut snap = snapshot.lock();
                    snap.seek(SeekFrom::Start((page_idx * PAGE_SIZE) as u64))?;
                    snap.write_all(page_slice)?;
                }
                // Clear WP for that page so the writer can proceed.
                uffd_raw::writeprotect(&uffd, page_addr as *mut _, PAGE_SIZE, false)
                    .map_err(|e| anyhow!("clear WP: {e}"))?;
                dirty_faults.fetch_add(1, Ordering::Relaxed);
            }
            Ok(())
        })
    };

    // 8. Writer thread.
    let writer = {
        let writes_done = Arc::clone(&writes_done);
        thread::spawn(move || {
            let mut rng = rand::thread_rng();
            let start = Instant::now();
            while start.elapsed() < WRITER_DURATION {
                let page_idx = rng.gen_range(0..num_pages);
                let label = after_label(page_idx);
                let label_bytes = label.as_bytes();
                let dest = (region_addr + page_idx * PAGE_SIZE) as *mut u8;
                unsafe {
                    std::ptr::copy_nonoverlapping(label_bytes.as_ptr(), dest, label_bytes.len());
                }
                writes_done.fetch_add(1, Ordering::Relaxed);
            }
        })
    };

    let scribble_start = Instant::now();
    writer.join().map_err(|_| anyhow!("writer panicked"))?;
    let scribble_elapsed = scribble_start.elapsed();
    let total_writes = writes_done.load(Ordering::Relaxed);
    println!(
        "[writer] {} writes in {:?} ({:.0} writes/sec)",
        total_writes,
        scribble_elapsed,
        total_writes as f64 / scribble_elapsed.as_secs_f64()
    );

    // Drain in-flight faults briefly.
    thread::sleep(Duration::from_millis(200));
    stop_handler.store(true, Ordering::Release);
    handler.join().map_err(|_| anyhow!("handler panicked"))??;
    let total_faults = dirty_faults.load(Ordering::Relaxed);
    println!("[handler] caught {} WP faults", total_faults);

    // 9. Bulk-copy clean pages.
    let bulk_start = Instant::now();
    let mut clean_copies = 0u64;
    {
        let mut snap = snapshot.lock();
        for page_idx in 0..num_pages {
            if !captured[page_idx].swap(true, Ordering::AcqRel) {
                let page_slice = unsafe {
                    std::slice::from_raw_parts(
                        (region_addr + page_idx * PAGE_SIZE) as *const u8,
                        PAGE_SIZE,
                    )
                };
                snap.seek(SeekFrom::Start((page_idx * PAGE_SIZE) as u64))?;
                snap.write_all(page_slice)?;
                clean_copies += 1;
            }
        }
    }
    println!(
        "[bulk] copied {clean_copies} still-clean pages in {:?}",
        bulk_start.elapsed()
    );

    // 10. Validate.
    let snap_data = std::fs::read(SNAPSHOT_FILE)?;
    if snap_data.len() != region_size {
        bail!(
            "snapshot file is {} bytes, expected {}",
            snap_data.len(),
            region_size
        );
    }
    let mut ok = 0usize;
    let mut violations: Vec<usize> = Vec::new();
    for page_idx in 0..num_pages {
        let prefix = &snap_data[page_idx * PAGE_SIZE..page_idx * PAGE_SIZE + 32];
        let expected = before_label(page_idx);
        if prefix.starts_with(expected.as_bytes()) {
            ok += 1;
        } else {
            violations.push(page_idx);
            if violations.len() <= 5 {
                let got = String::from_utf8_lossy(&prefix[..expected.len().min(32)]);
                eprintln!(
                    "[verify] page {page_idx} mismatch: expected {expected:?}, got {got:?}"
                );
            }
        }
    }

    println!("\n=== Result ===");
    println!("WP arm latency:           {:?}", wp_arm_elapsed);
    println!("Writer throughput:        {} writes in {:?}", total_writes, scribble_elapsed);
    println!("WP faults handled:        {}", total_faults);
    println!("Pages captured by fault:  {}", num_pages - clean_copies as usize);
    println!("Pages captured by bulk:   {}", clean_copies);
    println!("Snapshot pages ok:        {} / {}", ok, num_pages);
    println!("Snapshot violations:      {}", violations.len());

    if !violations.is_empty() {
        bail!(
            "PoC FAILED: {} snapshot pages contained post-WP-arm content",
            violations.len()
        );
    }
    println!("\nPoC PASSED — snapshot is a consistent point-in-time view.");

    unsafe {
        libc::munmap(region, region_size);
    }
    let _ = std::fs::remove_file(SNAPSHOT_FILE);
    Ok(())
}
