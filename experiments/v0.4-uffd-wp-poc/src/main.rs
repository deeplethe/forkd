//! v0.4 Phase 1 PoC: prove that `UFFDIO_WRITEPROTECT` on a memfd-backed VMA
//! delivers write-faults to a userspace handler, and that the handler can
//! capture the *pre-write* page content before the writer continues.
//!
//! What this exercises (and what v0.4 needs to be sound):
//!
//! 1. `memfd_create` + `mmap(MAP_SHARED)` — anonymous memory the kernel will
//!    let us write-protect via userfaultfd.
//! 2. `UFFDIO_REGISTER` with `MODE_WP` over the full region.
//! 3. `UFFDIO_WRITEPROTECT` to arm WP — we time this; it is the v0.4
//!    "pause window" analog.
//! 4. A writer thread that scribbles random pages.
//! 5. A handler thread that polls the uffd, copies each first-write page
//!    into a snapshot file at the right offset, then `remove_write_protection`s
//!    that single page so the writer can proceed.
//! 6. After the writer stops, a "bulk copier" pass to flush the still-clean
//!    pages to the snapshot file (still WP'd, safe to read directly).
//! 7. Validation: every page in the snapshot must contain the *BEFORE*
//!    pattern. If any AFTER value leaked in, the ordering invariant is
//!    broken and v0.4's correctness argument falls apart.
//!
//! Linux x86_64, kernel ≥ 5.7 (UFFDIO_WRITEPROTECT landed in 5.7 for
//! anonymous and shmem-backed VMAs). Run:
//!
//!     cargo run --release -p v0_4-uffd-wp-poc

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
use userfaultfd::{Event, FeatureFlags, RegisterMode, UffdBuilder};

const PAGE_SIZE: usize = 4096;
const REGION_SIZE: usize = 64 * 1024 * 1024; // 64 MiB — small enough to iterate fast, big enough to be real.
const NUM_PAGES: usize = REGION_SIZE / PAGE_SIZE;
const WRITER_DURATION: Duration = Duration::from_secs(3);
const SNAPSHOT_FILE: &str = "/tmp/v0.4-uffd-wp-poc.snapshot";

// Page-content patterns. We write a short ASCII label at the start of each
// page so the validator can tell BEFORE from AFTER.
fn before_label(page_idx: usize) -> String {
    format!("PAGE_{page_idx:06}_BEFORE")
}

fn after_label(page_idx: usize) -> String {
    format!("PAGE_{page_idx:06}_AFTER ")
}

fn main() -> Result<()> {
    println!("=== v0.4 Phase 1 PoC: UFFDIO_WRITEPROTECT on memfd ===");
    println!(
        "Region: {} MiB ({} pages of {} bytes)\n",
        REGION_SIZE / 1024 / 1024,
        NUM_PAGES,
        PAGE_SIZE
    );

    // 1. memfd + mmap.
    let memfd_name = std::ffi::CString::new("v0.4-poc")?;
    let memfd = memfd_create(&memfd_name, MemFdCreateFlag::MFD_CLOEXEC).context("memfd_create")?;
    nix::unistd::ftruncate(&memfd, REGION_SIZE as i64).context("ftruncate")?;

    let region = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            REGION_SIZE,
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
    for page_idx in 0..NUM_PAGES {
        let label = before_label(page_idx);
        let label_bytes = label.as_bytes();
        let dest = (region_addr + page_idx * PAGE_SIZE) as *mut u8;
        unsafe {
            std::ptr::copy_nonoverlapping(label_bytes.as_ptr(), dest, label_bytes.len());
        }
    }
    println!(
        "[setup] populated {} pages with BEFORE patterns in {:?}",
        NUM_PAGES,
        populate_start.elapsed()
    );

    // 3. Build userfaultfd, register with WP mode.
    let uffd = UffdBuilder::new()
        .close_on_exec(true)
        .non_blocking(false)
        .require_features(FeatureFlags::PAGEFAULT_FLAG_WP)
        .create()
        .context(
            "uffd create — kernel <5.7 lacks UFFD_WP; or unprivileged userfaultfd disabled \
             (try `sysctl vm.unprivileged_userfaultfd=1` or run as root)",
        )?;
    println!("[uffd] created uffd fd");

    let ioctls = uffd
        .register_with_mode(region, REGION_SIZE, RegisterMode::WRITE_PROTECT)
        .context("uffd register WP")?;
    println!("[uffd] registered region with WRITE_PROTECT, supported ioctls: {ioctls:?}");

    // 4. Arm WP — the moment in v0.4 that is the BRANCH "pause window" analog.
    let wp_arm_start = Instant::now();
    uffd.write_protect(region, REGION_SIZE)
        .context("uffd write_protect arm")?;
    let wp_arm_elapsed = wp_arm_start.elapsed();
    println!(
        "[wp] armed UFFDIO_WRITEPROTECT over {} MiB in {:?}  ← v0.4 pause-window analog",
        REGION_SIZE / 1024 / 1024,
        wp_arm_elapsed
    );

    // 5. Open snapshot file, pre-allocate.
    let snapshot = Arc::new(Mutex::new(
        OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(SNAPSHOT_FILE)?,
    ));
    snapshot.lock().set_len(REGION_SIZE as u64)?;

    // 6. Shared state.
    // `captured[i] = true` once page i has been written into the snapshot
    // (either by the WP handler on its first fault, or by the bulk copier).
    // Either path is exactly-once; whichever wins the CAS owns the write.
    let captured: Arc<Vec<AtomicBool>> = Arc::new((0..NUM_PAGES).map(|_| AtomicBool::new(false)).collect());
    let dirty_faults = Arc::new(AtomicU64::new(0));
    let writes_done = Arc::new(AtomicU64::new(0));
    let stop_handler = Arc::new(AtomicBool::new(false));

    // 7. Handler thread — polls uffd, captures pages on first WP fault,
    //    then removes WP for that page so the writer can proceed.
    let uffd_arc = Arc::new(uffd);
    let handler = {
        let uffd = Arc::clone(&uffd_arc);
        let captured = Arc::clone(&captured);
        let dirty_faults = Arc::clone(&dirty_faults);
        let snapshot = Arc::clone(&snapshot);
        let stop_handler = Arc::clone(&stop_handler);
        thread::spawn(move || -> Result<()> {
            while !stop_handler.load(Ordering::Acquire) {
                let event = match uffd.read_event_timeout(Duration::from_millis(50)) {
                    Ok(Some(ev)) => ev,
                    Ok(None) => continue,
                    Err(e) => {
                        eprintln!("[handler] uffd read error: {e}");
                        break;
                    }
                };
                if let Event::Pagefault { addr, .. } = event {
                    let page_addr = (addr as usize) & !(PAGE_SIZE - 1);
                    let page_idx = (page_addr - region_addr) / PAGE_SIZE;
                    // CAS: only the first claimant writes the snapshot.
                    if !captured[page_idx].swap(true, Ordering::AcqRel) {
                        let page_slice =
                            unsafe { std::slice::from_raw_parts(page_addr as *const u8, PAGE_SIZE) };
                        let mut snap = snapshot.lock();
                        snap.seek(SeekFrom::Start((page_idx * PAGE_SIZE) as u64))?;
                        snap.write_all(page_slice)?;
                    }
                    // Either way, clear WP for this page so the faulting writer can proceed.
                    uffd.remove_write_protection(page_addr as *mut _, PAGE_SIZE, true)
                        .map_err(|e| anyhow!("remove_write_protection: {e}"))?;
                    dirty_faults.fetch_add(1, Ordering::Relaxed);
                }
            }
            Ok(())
        })
    };

    // 8. Writer thread — scribbles random pages with AFTER labels for WRITER_DURATION.
    let writer = {
        let writes_done = Arc::clone(&writes_done);
        thread::spawn(move || {
            let mut rng = rand::thread_rng();
            let start = Instant::now();
            while start.elapsed() < WRITER_DURATION {
                let page_idx = rng.gen_range(0..NUM_PAGES);
                let label = after_label(page_idx);
                let label_bytes = label.as_bytes();
                let dest = (region_addr + page_idx * PAGE_SIZE) as *mut u8;
                unsafe {
                    // This write may fault; the handler will catch it, copy
                    // the BEFORE page to the snapshot, clear WP, and the
                    // write retries successfully.
                    std::ptr::copy_nonoverlapping(label_bytes.as_ptr(), dest, label_bytes.len());
                }
                writes_done.fetch_add(1, Ordering::Relaxed);
            }
        })
    };

    let scribble_start = Instant::now();
    writer.join().map_err(|_| anyhow!("writer thread panicked"))?;
    let scribble_elapsed = scribble_start.elapsed();
    println!(
        "[writer] done: {} writes in {:?} ({:.0} writes/sec)",
        writes_done.load(Ordering::Relaxed),
        scribble_elapsed,
        writes_done.load(Ordering::Relaxed) as f64 / scribble_elapsed.as_secs_f64()
    );

    // Give the handler a moment to drain any in-flight faults from the writer's last writes.
    thread::sleep(Duration::from_millis(100));
    stop_handler.store(true, Ordering::Release);
    handler
        .join()
        .map_err(|_| anyhow!("handler thread panicked"))?
        .context("handler exit")?;
    println!(
        "[handler] caught {} WP faults",
        dirty_faults.load(Ordering::Relaxed)
    );

    // 9. Bulk-copy remaining clean pages into the snapshot.
    //    Since the writer is stopped and these pages are still WP'd
    //    (no fault was ever observed for them), reading them gives the
    //    untouched BEFORE content.
    let bulk_start = Instant::now();
    let mut clean_copies = 0u64;
    {
        let mut snap = snapshot.lock();
        for page_idx in 0..NUM_PAGES {
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

    // 10. Validate: every page in the snapshot must start with its BEFORE label.
    //     If any page starts with AFTER, the WP ordering invariant is violated
    //     and v0.4's snapshot consistency claim is false.
    let snap_data = std::fs::read(SNAPSHOT_FILE)?;
    if snap_data.len() != REGION_SIZE {
        bail!(
            "snapshot file is {} bytes, expected {}",
            snap_data.len(),
            REGION_SIZE
        );
    }
    let mut ok = 0usize;
    let mut violations: Vec<usize> = Vec::new();
    for page_idx in 0..NUM_PAGES {
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
    println!("WP arm latency:          {:?}", wp_arm_elapsed);
    println!("Writer throughput:       {} writes in {:?}", writes_done.load(Ordering::Relaxed), scribble_elapsed);
    println!("WP faults handled:       {}", dirty_faults.load(Ordering::Relaxed));
    println!("Pages captured by fault: {}", NUM_PAGES - clean_copies as usize);
    println!("Pages captured by bulk:  {}", clean_copies);
    println!("Snapshot pages ok:       {} / {}", ok, NUM_PAGES);
    println!("Snapshot violations:     {}", violations.len());

    if !violations.is_empty() {
        bail!(
            "PoC FAILED: {} snapshot pages contained post-WP-arm content (consistency violated)",
            violations.len()
        );
    }
    println!("\nPoC PASSED — snapshot is a consistent point-in-time view.");

    // Cleanup mmap.
    unsafe {
        libc::munmap(region, REGION_SIZE);
    }
    let _ = std::fs::remove_file(SNAPSHOT_FILE);
    Ok(())
}
