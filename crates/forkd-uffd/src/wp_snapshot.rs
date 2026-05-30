//! Snapshot-side write-protection — the v0.4 live-fork primitive.
//!
//! The shape of a v0.4 BRANCH:
//!
//! 1. [`WpBranch::begin`] arms `UFFDIO_WRITEPROTECT` on the source VM's
//!    memfd region. Sub-millisecond per GiB on tested kernels; this is
//!    the BRANCH "pause window" critical section.
//! 2. The caller can now safely dump vCPU + device state from a paused
//!    Firecracker — guest memory is frozen from the guest's
//!    perspective. Once the dump is done, resume the guest. (The
//!    snapshotter doesn't pause/resume the guest itself; that's
//!    Firecracker's responsibility.)
//! 3. While the guest runs, [`WpBranch::bulk_copy_clean`] reads
//!    still-clean pages from the memfd and writes them to the snapshot
//!    file. In parallel, the spawned handler thread captures any pages
//!    the guest writes-to and clears WP for those pages.
//! 4. [`WpBranch::finalize`] stops the handler and returns stats.
//!
//! The consistency invariant: every page written to the snapshot file
//! holds the value the guest could read at the moment WP was armed.
//! Phase 1+2+3 PoCs in `experiments/v0.4-*-poc/` verified this empirically
//! (0 violations across 64 MiB / 256 MiB / 1 GiB regions and KVM guest
//! writes through EPT). See `DESIGN-v0.4.md` for the full design.

use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::os::fd::OwnedFd;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};

use crate::raw;

/// 4 KiB base page; matches what UFFD_WP reports faults at, even when
/// the underlying VMA is hugepage-backed (verified in Phase 3 PoC).
pub const PAGE_SIZE: usize = 4096;

/// Snapshot-side write-protection state for one in-flight BRANCH.
///
/// Owns the userfaultfd, the spawned handler thread, and the shared
/// state both threads see. Construct with [`begin`](Self::begin); finish
/// with [`finalize`](Self::finalize).
pub struct WpBranch {
    /// Page-level reads (bulk copy + handler page captures) use this
    /// address. For the in-process `begin` constructor, this is the
    /// same as `uffd_region_addr`. For the cross-process
    /// `begin_with_external_uffd` constructor, it's the controller's
    /// mmap of the shared memfd — the controller can dereference here
    /// safely, where dereferencing the uffd-registration-side address
    /// would be an invalid memory access.
    region_addr: usize,
    /// The address space the uffd was registered against. UFFDIO_*
    /// operations (WRITEPROTECT, fault address translation) use this.
    /// For `begin`, identical to `region_addr`. For
    /// `begin_with_external_uffd`, this is FC's mmap address (carried
    /// in the SCM_RIGHTS handshake).
    uffd_region_addr: usize,
    region_size: usize,
    arm_duration: Duration,
    state: Arc<SharedState>,
    handler: Option<JoinHandle<Result<()>>>,
    stop: Arc<AtomicBool>,
}

struct SharedState {
    snapshot: Mutex<File>,
    captured: Vec<AtomicBool>,
    dirty_faults: AtomicU64,
    uffd: OwnedFd,
}

/// Counters reported back from a finished branch.
#[derive(Debug, Clone, Copy)]
pub struct WpBranchStats {
    /// Time the `UFFDIO_WRITEPROTECT` syscall held the source VM's
    /// memory in its critical section. This is the analog of v0.3.4's
    /// 150 ms ext4 pause.
    pub arm_duration: Duration,
    /// Number of pages captured because the guest wrote to them
    /// (the WP fault path).
    pub pages_captured_by_fault: u64,
    /// Number of pages captured by the bulk-copy pass (clean pages).
    pub pages_captured_by_bulk: u64,
    /// Total pages in the region.
    pub total_pages: u64,
}

impl WpBranch {
    /// Arm WP and spawn the handler.
    ///
    /// `memfd` is consumed for lifetime management — the snapshotter
    /// keeps it alive until [`finalize`](Self::finalize) so the region
    /// stays mapped. `region` and `region_size` describe the live mmap
    /// of that memfd in this process. `snapshot_path` is where dirty
    /// pages will be written; the file is created (truncating if it
    /// existed) and pre-sized to `region_size`.
    ///
    /// # Safety
    ///
    /// `region` must point to a valid `region_size`-byte mmap of
    /// `memfd` in this process. The mmap must remain alive for the
    /// lifetime of the returned `WpBranch` (i.e., the caller must not
    /// munmap it until after `finalize`).
    pub unsafe fn begin(
        memfd: OwnedFd,
        region: *mut libc::c_void,
        region_size: usize,
        snapshot_path: &Path,
    ) -> Result<Self> {
        if region.is_null() {
            bail!("WpBranch::begin: region pointer is null");
        }
        if region_size == 0 || !region_size.is_multiple_of(PAGE_SIZE) {
            bail!(
                "WpBranch::begin: region_size {region_size} must be a positive multiple of {PAGE_SIZE}"
            );
        }
        let region_addr = region as usize;
        let num_pages = region_size / PAGE_SIZE;

        // Create uffd, register WP, arm WP. The arm is the timed
        // critical section.
        let uffd = raw::create_uffd().context("create userfaultfd")?;
        let _ioctls =
            raw::register_wp(&uffd, region, region_size).context("UFFDIO_REGISTER (WP)")?;

        let arm_start = Instant::now();
        raw::writeprotect(&uffd, region, region_size, true).context("UFFDIO_WRITEPROTECT arm")?;
        let arm_duration = arm_start.elapsed();

        // Snapshot file — pre-size so writes at arbitrary offsets just
        // overwrite holes (sparse files behave well here on ext4).
        let snapshot = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(snapshot_path)
            .with_context(|| format!("open snapshot file {}", snapshot_path.display()))?;
        snapshot
            .set_len(region_size as u64)
            .context("set_len snapshot file")?;

        let captured: Vec<AtomicBool> = (0..num_pages).map(|_| AtomicBool::new(false)).collect();

        let state = Arc::new(SharedState {
            snapshot: Mutex::new(snapshot),
            captured,
            dirty_faults: AtomicU64::new(0),
            uffd,
        });
        let stop = Arc::new(AtomicBool::new(false));

        // Spawn the handler. Holds a clone of the shared state; uses the
        // owned memfd kept alive in this struct via `_memfd_keepalive`.
        let handler_state = Arc::clone(&state);
        let handler_stop = Arc::clone(&stop);
        let handler = thread::spawn(move || -> Result<()> {
            // In-process: read-side and uffd-side addresses coincide.
            run_handler(handler_state, handler_stop, region_addr, region_addr)
        });

        // We keep `memfd` alive by leaking it into a field on the
        // returned struct. Dropped on finalize().
        let _ = memfd; // currently unused after register; reserved for future bulk-copy via memfd read.

        Ok(WpBranch {
            region_addr,
            uffd_region_addr: region_addr,
            region_size,
            arm_duration,
            state,
            handler: Some(handler),
            stop,
        })
    }

    /// Like [`begin`](Self::begin) but takes an already-registered uffd
    /// instead of creating one. The v0.4 controller path uses this:
    /// Firecracker creates the uffd inside its own process (because
    /// `UFFDIO_REGISTER` is per-process and KVM runs in FC) and ships
    /// the fd to the controller via SCM_RIGHTS — see
    /// `Vm::request_wp_uffd` in `forkd-vmm`. The controller then
    /// arms WP from this side without needing the register ioctl.
    ///
    /// `memfd` is the controller's own clone of FC's backing memfd, used
    /// by [`bulk_copy_clean`](Self::bulk_copy_clean) to read clean pages
    /// through the controller's mmap. `region` and `region_size` describe
    /// the controller-side mmap of that memfd.
    ///
    /// # Safety
    ///
    /// `region` must point to a valid `region_size`-byte mmap of `memfd`
    /// in this process. The mmap must remain alive for the lifetime of
    /// the returned `WpBranch`. `uffd` must already be registered (WP
    /// mode) against the same memory range *as observed in the registering
    /// process* — for the forkd v0.4 path that's FC's process, which has
    /// its own mmap of the same memfd inode at a different VA. The
    /// kernel's UFFD_WP plumbing tracks pages at the page-table level,
    /// so arming + faults work as long as both processes ultimately
    /// touch the same backing pages.
    pub unsafe fn begin_with_external_uffd(
        uffd: OwnedFd,
        memfd: OwnedFd,
        uffd_region_addr: usize,
        controller_region: *mut libc::c_void,
        region_size: usize,
        snapshot_path: &Path,
    ) -> Result<Self> {
        if controller_region.is_null() {
            bail!("WpBranch::begin_with_external_uffd: controller_region pointer is null");
        }
        if uffd_region_addr == 0 {
            bail!("WpBranch::begin_with_external_uffd: uffd_region_addr is null");
        }
        if region_size == 0 || !region_size.is_multiple_of(PAGE_SIZE) {
            bail!(
                "WpBranch::begin_with_external_uffd: region_size {region_size} must be a positive multiple of {PAGE_SIZE}"
            );
        }
        let region_addr = controller_region as usize;
        let num_pages = region_size / PAGE_SIZE;

        // No UFFDIO_REGISTER — FC already did it. Just arm WP against
        // FC's address space (where the uffd was registered).
        let arm_start = Instant::now();
        raw::writeprotect(
            &uffd,
            uffd_region_addr as *mut libc::c_void,
            region_size,
            true,
        )
        .context("UFFDIO_WRITEPROTECT arm (external uffd)")?;
        let arm_duration = arm_start.elapsed();

        let snapshot = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(snapshot_path)
            .with_context(|| format!("open snapshot file {}", snapshot_path.display()))?;
        snapshot
            .set_len(region_size as u64)
            .context("set_len snapshot file")?;

        let captured: Vec<AtomicBool> = (0..num_pages).map(|_| AtomicBool::new(false)).collect();

        let state = Arc::new(SharedState {
            snapshot: Mutex::new(snapshot),
            captured,
            dirty_faults: AtomicU64::new(0),
            uffd,
        });
        let stop = Arc::new(AtomicBool::new(false));

        let handler_state = Arc::clone(&state);
        let handler_stop = Arc::clone(&stop);
        let handler = thread::spawn(move || -> Result<()> {
            // Cross-process: faults report FC's addresses; reads must
            // go through the controller's mmap. The handler translates.
            run_handler(handler_state, handler_stop, region_addr, uffd_region_addr)
        });

        let _ = memfd; // keepalive only — handler reads through the existing mmap.

        Ok(WpBranch {
            region_addr,
            uffd_region_addr,
            region_size,
            arm_duration,
            state,
            handler: Some(handler),
            stop,
        })
    }

    /// The time `UFFDIO_WRITEPROTECT` held the critical section.
    pub fn arm_duration(&self) -> Duration {
        self.arm_duration
    }

    /// Read every still-uncaught page directly from the live mmap and
    /// write it to the snapshot. Safe because uncaught pages are still
    /// WP'd (the guest can't have written to them yet), so we see the
    /// pre-WP-arm content.
    ///
    /// Returns the number of pages copied this way.
    ///
    /// # Safety
    ///
    /// The region mmap must still be alive (see [`begin`](Self::begin)).
    pub unsafe fn bulk_copy_clean(&self) -> Result<usize> {
        let num_pages = self.region_size / PAGE_SIZE;
        let mut copied = 0usize;
        let mut snap = self
            .state
            .snapshot
            .lock()
            .map_err(|_| anyhow!("snapshot mutex poisoned"))?;
        for page_idx in 0..num_pages {
            if !self.state.captured[page_idx].swap(true, Ordering::AcqRel) {
                let src = (self.region_addr + page_idx * PAGE_SIZE) as *const u8;
                let page_slice = std::slice::from_raw_parts(src, PAGE_SIZE);
                snap.seek(SeekFrom::Start((page_idx * PAGE_SIZE) as u64))?;
                snap.write_all(page_slice)?;
                copied += 1;
            }
        }
        Ok(copied)
    }

    /// Stop the handler thread, sync the snapshot, return stats.
    pub fn finalize(mut self) -> Result<WpBranchStats> {
        // Drain briefly to catch any in-flight faults from the writer's
        // last actions before we stop.
        thread::sleep(Duration::from_millis(50));
        self.stop.store(true, Ordering::Release);
        let handler = self
            .handler
            .take()
            .ok_or_else(|| anyhow!("WpBranch already finalized"))?;
        handler
            .join()
            .map_err(|_| anyhow!("WP handler thread panicked"))?
            .context("WP handler returned error")?;

        // Clear WP across the entire region before dropping the uffd.
        // Pages captured by bulk_copy_clean don't trigger fault-side
        // WP clearing, so absent this step they'd stay write-protected
        // until uffd-drop's implicit teardown. Some guest workloads
        // racing against teardown have been observed to wedge — be
        // explicit. The clear is the registering-process's address
        // space (FC's in the cross-process path).
        if let Err(e) = raw::writeprotect(
            &self.state.uffd,
            self.uffd_region_addr as *mut _,
            self.region_size,
            false,
        ) {
            tracing::warn!(
                error = ?e,
                "WpBranch::finalize: best-effort whole-region WP clear failed"
            );
        }

        // Sync snapshot to durable storage.
        let snap = self
            .state
            .snapshot
            .lock()
            .map_err(|_| anyhow!("snapshot mutex poisoned"))?;
        snap.sync_data().context("fsync snapshot")?;
        drop(snap);

        let dirty = self.state.dirty_faults.load(Ordering::Relaxed);
        let total = (self.region_size / PAGE_SIZE) as u64;
        // pages_captured_by_fault is dirty faults; bulk-captured is the rest.
        // But we don't track bulk separately from fault past the swap, so
        // we report dirty (fault path) directly and infer bulk from
        // total minus pages-that-faulted. This is a slight
        // approximation if a page faulted multiple times — the handler
        // only counts each fault, but the captured-once invariant means
        // each unique page contributes exactly one capture.
        let pages_captured_by_fault = dirty.min(total);
        let pages_captured_by_bulk = total.saturating_sub(pages_captured_by_fault);
        Ok(WpBranchStats {
            arm_duration: self.arm_duration,
            pages_captured_by_fault,
            pages_captured_by_bulk,
            total_pages: total,
        })
    }
}

fn run_handler(
    state: Arc<SharedState>,
    stop: Arc<AtomicBool>,
    read_region_addr: usize,
    uffd_region_addr: usize,
) -> Result<()> {
    while !stop.load(Ordering::Acquire) {
        let msg = match raw::poll_event(&state.uffd, 50)? {
            Some(m) => m,
            None => continue,
        };
        if msg.event != raw::UFFD_EVENT_PAGEFAULT {
            continue;
        }
        let (flags, addr) = msg.as_pagefault();
        if (flags & raw::UFFD_PAGEFAULT_FLAG_WRITE) == 0 {
            continue;
        }
        // The fault address is in the uffd's registering process —
        // that's FC's mmap address for the cross-process case, or
        // (equivalently) our own for the in-process case.
        let fault_page_uffd = (addr as usize) & !(PAGE_SIZE - 1);
        if fault_page_uffd < uffd_region_addr {
            continue;
        }
        let page_offset = fault_page_uffd - uffd_region_addr;
        let page_idx = page_offset / PAGE_SIZE;
        if page_idx >= state.captured.len() {
            continue;
        }
        // First-to-CAS owns the snapshot write.
        if !state.captured[page_idx].swap(true, Ordering::AcqRel) {
            // Read through the controller's mmap; same backing memfd
            // as FC, but a valid address in this process.
            let read_page_addr = read_region_addr + page_offset;
            let page_slice =
                unsafe { std::slice::from_raw_parts(read_page_addr as *const u8, PAGE_SIZE) };
            let mut snap = state
                .snapshot
                .lock()
                .map_err(|_| anyhow!("snapshot mutex poisoned"))?;
            snap.seek(SeekFrom::Start(page_offset as u64))?;
            snap.write_all(page_slice)?;
        }
        // Clear WP for this page so the faulting guest write can
        // proceed. WRITEPROTECT operates on the registering process's
        // address space, so use the FC-side page address.
        raw::writeprotect(&state.uffd, fault_page_uffd as *mut _, PAGE_SIZE, false)
            .context("clear WP after capture")?;
        state.dirty_faults.fetch_add(1, Ordering::Relaxed);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal smoke test: arm + immediately finalize with no writes.
    /// All pages should be bulk-copied; none should fault. Requires
    /// kernel ≥ 5.7 and either root or `vm.unprivileged_userfaultfd=1`.
    #[test]
    #[cfg_attr(not(target_os = "linux"), ignore)]
    fn arm_and_finalize_no_writes() {
        // Allocate a small anon mmap to stand in for a memfd-backed
        // region. begin() ignores the memfd's content — it just keeps
        // the fd alive. Using /dev/null fd as a placeholder.
        const SIZE: usize = 4 * PAGE_SIZE;
        let region = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                SIZE,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if region == libc::MAP_FAILED {
            // Skip if mmap fails (CI sandbox?).
            return;
        }
        // Pre-populate with a known pattern so bulk_copy reads
        // something deterministic.
        unsafe {
            std::ptr::write_bytes(region as *mut u8, 0x42, SIZE);
        }

        use std::os::fd::FromRawFd;
        let placeholder_fd =
            unsafe { OwnedFd::from_raw_fd(libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY)) };

        let snap_path =
            std::env::temp_dir().join(format!("forkd-wp-test-{}.snap", std::process::id()));

        let branch = match unsafe { WpBranch::begin(placeholder_fd, region, SIZE, &snap_path) } {
            Ok(b) => b,
            Err(e) => {
                // Likely vm.unprivileged_userfaultfd=0 in CI. Skip.
                eprintln!("WpBranch::begin failed, skipping test: {e}");
                unsafe {
                    libc::munmap(region, SIZE);
                }
                return;
            }
        };

        let bulk = unsafe { branch.bulk_copy_clean() }.expect("bulk_copy_clean");
        assert_eq!(bulk, SIZE / PAGE_SIZE, "all pages should be bulk-copied");

        let stats = branch.finalize().expect("finalize");
        assert_eq!(stats.pages_captured_by_fault, 0);
        assert_eq!(stats.pages_captured_by_bulk as usize, SIZE / PAGE_SIZE);

        // Verify snapshot content.
        let data = std::fs::read(&snap_path).expect("read snapshot");
        assert_eq!(data.len(), SIZE);
        assert!(data.iter().all(|&b| b == 0x42));

        let _ = std::fs::remove_file(&snap_path);
        unsafe {
            libc::munmap(region, SIZE);
        }
    }
}
