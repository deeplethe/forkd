//! In-memory VM registry, snapshotted to a JSON file for crash recovery.
//!
//! Concurrency: one `parking_lot::Mutex` wraps the in-memory registry and a
//! second serializes atomic persistence. Writes are infrequent (one per
//! sandbox lifecycle event) so contention is a non-issue at our scale
//! (≤ a few thousand sandboxes/host).
//!
//! On startup, the daemon reads `state.json`, then reconciles each entry
//! against the live system (does the netns still exist, is the FC pid
//! still alive). Stale entries get pruned. See `Registry::reconcile`.
use anyhow::{Context, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use crate::api::{SandboxInfo, SnapshotInfo, WorkspaceInfo, WorkspaceStatus};

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct PersistentState {
    #[serde(default)]
    pub snapshots: BTreeMap<String, SnapshotInfo>,
    #[serde(default)]
    pub sandboxes: BTreeMap<String, SandboxInfo>,
    /// Stateful workspaces (#116). Keyed by name (user-facing
    /// identifier; unique per daemon). The internal `id` field on
    /// `WorkspaceInfo` is for audit / cross-reference with live sandbox
    /// pids; lookups + mutations from the HTTP / CLI surface go by name.
    #[serde(default)]
    pub workspaces: BTreeMap<String, WorkspaceInfo>,
}

#[derive(Clone)]
pub struct Registry {
    inner: Arc<Mutex<PersistentState>>,
    persist_lock: Arc<Mutex<()>>,
    path: PathBuf,
}

impl Registry {
    pub fn load_or_init(path: impl Into<PathBuf>) -> Result<Self> {
        let path: PathBuf = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create state dir {}", parent.display()))?;
        }
        let state = if path.exists() {
            let raw = fs::read_to_string(&path)
                .with_context(|| format!("read state file {}", path.display()))?;
            serde_json::from_str(&raw)
                .with_context(|| format!("parse state file {}", path.display()))?
        } else {
            PersistentState::default()
        };
        Ok(Self {
            inner: Arc::new(Mutex::new(state)),
            persist_lock: Arc::new(Mutex::new(())),
            path,
        })
    }

    pub fn snapshot(&self) -> PersistentState {
        self.inner.lock().clone()
    }

    pub fn list_snapshots(&self) -> Vec<SnapshotInfo> {
        self.inner.lock().snapshots.values().cloned().collect()
    }

    pub fn list_sandboxes(&self) -> Vec<SandboxInfo> {
        self.inner.lock().sandboxes.values().cloned().collect()
    }

    pub fn get_snapshot(&self, tag: &str) -> Option<SnapshotInfo> {
        self.inner.lock().snapshots.get(tag).cloned()
    }

    pub fn get_sandbox(&self, id: &str) -> Option<SandboxInfo> {
        self.inner.lock().sandboxes.get(id).cloned()
    }

    pub fn insert_snapshot(&self, snap: SnapshotInfo) -> Result<()> {
        {
            let mut g = self.inner.lock();
            g.snapshots.insert(snap.tag.clone(), snap);
        }
        self.flush()
    }

    pub fn insert_sandbox(&self, sb: SandboxInfo) -> Result<()> {
        {
            let mut g = self.inner.lock();
            g.sandboxes.insert(sb.id.clone(), sb);
        }
        self.flush()
    }

    /// Mark a sandbox as having been BRANCHed at least once, and
    /// record its most recent BRANCH output's memory.bin path so
    /// subsequent diff BRANCHes can chain off it (phase 1d).
    ///
    /// Returns Ok(Some(new_branch_count)) on success, where the count is
    /// the post-increment value (so the first BRANCH on a sandbox
    /// returns 1). Returns Ok(None) if the sandbox is no longer
    /// registered (it got DELETE'd during the BRANCH window —
    /// best-effort, the updated state is silently dropped).
    pub fn mark_branched(&self, id: &str, last_memory_bin: PathBuf) -> Result<Option<u32>> {
        let new_count = {
            let mut g = self.inner.lock();
            match g.sandboxes.get_mut(id) {
                Some(sb) => {
                    sb.has_branched = true;
                    sb.last_branch_memory_path = Some(last_memory_bin);
                    sb.branch_count = sb.branch_count.saturating_add(1);
                    Some(sb.branch_count)
                }
                None => None,
            }
        };
        if new_count.is_some() {
            self.flush()?;
        }
        Ok(new_count)
    }

    pub fn remove_sandbox(&self, id: &str) -> Result<Option<SandboxInfo>> {
        let removed = {
            let mut g = self.inner.lock();
            g.sandboxes.remove(id)
        };
        if removed.is_some() {
            self.flush()?;
        }
        Ok(removed)
    }

    pub fn remove_snapshot(&self, tag: &str) -> Result<Option<SnapshotInfo>> {
        let removed = {
            let mut g = self.inner.lock();
            g.snapshots.remove(tag)
        };
        if removed.is_some() {
            self.flush()?;
        }
        Ok(removed)
    }

    // -------------------------- workspaces (#116) --------------------------

    pub fn list_workspaces(&self) -> Vec<WorkspaceInfo> {
        self.inner.lock().workspaces.values().cloned().collect()
    }

    pub fn get_workspace(&self, name: &str) -> Option<WorkspaceInfo> {
        self.inner.lock().workspaces.get(name).cloned()
    }

    pub fn insert_workspace(&self, ws: WorkspaceInfo) -> Result<()> {
        {
            let mut g = self.inner.lock();
            g.workspaces.insert(ws.name.clone(), ws);
        }
        self.flush()
    }

    pub fn remove_workspace(&self, name: &str) -> Result<Option<WorkspaceInfo>> {
        let removed = {
            let mut g = self.inner.lock();
            g.workspaces.remove(name)
        };
        if removed.is_some() {
            self.flush()?;
        }
        Ok(removed)
    }

    /// Update a workspace in-place via a mutation closure. Returns
    /// Ok(true) if the workspace was found and the change persisted;
    /// Ok(false) if no such workspace.
    pub fn update_workspace<F>(&self, name: &str, mutate: F) -> Result<bool>
    where
        F: FnOnce(&mut WorkspaceInfo),
    {
        let updated = {
            let mut g = self.inner.lock();
            match g.workspaces.get_mut(name) {
                Some(ws) => {
                    mutate(ws);
                    true
                }
                None => false,
            }
        };
        if updated {
            self.flush()?;
        }
        Ok(updated)
    }

    /// Persist current state atomically (write to temp + rename).
    fn flush(&self) -> Result<()> {
        // Every Registry clone targets the same temp path. Keep snapshot,
        // write, and rename under one lock so concurrent lifecycle mutations
        // cannot race the shared state.json.tmp or let an older snapshot
        // overwrite a newer one.
        let _persist_guard = self.persist_lock.lock();
        let state = self.inner.lock().clone();
        let tmp = self.path.with_extension("json.tmp");
        let body = serde_json::to_vec_pretty(&state)?;
        fs::write(&tmp, &body)
            .with_context(|| format!("write tmp state file {}", tmp.display()))?;
        fs::rename(&tmp, &self.path)
            .with_context(|| format!("rename {} → {}", tmp.display(), self.path.display()))?;
        Ok(())
    }

    /// Prune sandbox entries whose recorded pid is no longer alive, and
    /// mark any `Running` workspace whose sandbox was just pruned as
    /// `Stale` (#116): the daemon crashed/restarted out from under it.
    /// Suspended workspaces are untouched; they were intentionally
    /// parked. Snapshots are kept regardless (they're disk artifacts).
    ///
    /// Runs the whole in-memory pass under one registry lock (#310):
    /// `pid_alive` is a `/proc` stat, cheap and free of any lock we
    /// care about, so holding the mutex across it is fine and removes
    /// the window where an HTTP handler could remove/reinsert a
    /// just-observed-stale id between the scan and the prune.
    pub fn reconcile(&self) -> Result<usize> {
        let (pruned, stale_ws_changed) = {
            let mut g = self.inner.lock();
            let stale_ids: Vec<String> = g
                .sandboxes
                .iter()
                .filter_map(|(id, sb)| match sb.pid {
                    Some(pid) if !pid_alive(pid) => Some(id.clone()),
                    _ => None,
                })
                .collect();
            for id in &stale_ids {
                g.sandboxes.remove(id);
            }

            let live_ids: std::collections::HashSet<String> = g.sandboxes.keys().cloned().collect();
            let mut stale_ws_changed = false;
            for ws in g.workspaces.values_mut() {
                if ws.status == WorkspaceStatus::Running {
                    let live = ws
                        .live_sandbox_id
                        .as_ref()
                        .is_some_and(|id| live_ids.contains(id));
                    if !live {
                        ws.status = WorkspaceStatus::Stale;
                        ws.live_sandbox_id = None;
                        stale_ws_changed = true;
                    }
                }
            }

            (stale_ids.len(), stale_ws_changed)
        };

        if pruned > 0 || stale_ws_changed {
            self.flush()?;
        }
        Ok(pruned)
    }

    /// Mark any `Running` workspace whose `live_sandbox_id` is no longer
    /// present in `sandboxes` as `Stale`. We don't touch Suspended
    /// workspaces; they were intentionally parked.
    ///
    /// Used by `kill_orphans` (after pruning orphaned processes) to keep
    /// the workspace/liveness view consistent. Returns whether anything
    /// changed.
    fn mark_stale_workspaces(&self) -> bool {
        let live_ids: std::collections::HashSet<String> =
            self.inner.lock().sandboxes.keys().cloned().collect();
        let mut changed = false;
        {
            let mut g = self.inner.lock();
            for ws in g.workspaces.values_mut() {
                if ws.status == WorkspaceStatus::Running {
                    let live = ws
                        .live_sandbox_id
                        .as_ref()
                        .is_some_and(|id| live_ids.contains(id));
                    if !live {
                        ws.status = WorkspaceStatus::Stale;
                        ws.live_sandbox_id = None;
                        changed = true;
                    }
                }
            }
        }
        changed
    }

    /// Kill orphaned Firecracker processes on startup (issue #298).
    ///
    /// **MUST only be called before any `Vm` handle is held** (i.e. in
    /// `run_daemon` before `live_vms` is populated). This method
    /// SIGKILLs every sandbox entry whose PID is alive — it cannot
    /// distinguish an orphaned VM from a currently managed one.
    ///
    /// After `reconcile()` prunes entries with dead PIDs, any remaining
    /// sandbox entries have alive PIDs — but the controller has no
    /// `live_vms` handle for them (the controller restarted). These are
    /// orphaned Firecracker processes: alive but unmanageable.
    ///
    /// For each orphan: verify the PID still belongs to a Firecracker
    /// process (guards against PID reuse), send SIGKILL, wait for the
    /// process to exit (bounded timeout), then prune the registry entry.
    /// If the kill fails with a real error (not ESRCH), the entry is
    /// **not** pruned — a live orphan holding resources must stay
    /// registered so the operator can investigate.
    ///
    /// After this, the NetnsAllocator (empty active set) and
    /// shared_tap_owner (None) are safe once all orphans are confirmed
    /// dead: no orphaned VM holds a netns index or the shared tap.
    /// If some kills failed, those entries remain in the registry and
    /// the allocator may still collide with them — the caller should
    /// log the failure count and the operator should investigate.
    /// The work dir a Firecracker process was started with, read from its own
    /// `--api-sock` argument. `None` when the process is already gone or the
    /// argument is unreadable — callers skip the reclaim rather than guess a
    /// path, since removing the wrong directory would take a live VM's sockets
    /// with it.
    fn child_work_dir_from_cmdline(pid: u32) -> Option<std::path::PathBuf> {
        let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
        let args: Vec<String> = raw
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect();
        let sock = args
            .iter()
            .position(|a| a == "--api-sock")
            .and_then(|i| args.get(i + 1))?;
        Some(std::path::Path::new(sock).parent()?.to_path_buf())
    }

    /// Is `name` a restored child's rootfs backing (`child-<n>.<stem>`)?
    ///
    /// The same dir also holds `child-<n>.sock` and `child-<n>.console`, and the
    /// stem is the tag's rootfs filename — not always `rootfs.ext4` — so match
    /// on shape rather than a fixed suffix.
    fn is_child_backing(name: &str) -> bool {
        let Some(rest) = name.strip_prefix("child-") else {
            return false;
        };
        let Some((index, ext)) = rest.split_once('.') else {
            return false;
        };
        !index.is_empty()
            && index.chars().all(|c| c.is_ascii_digit())
            && !ext.is_empty()
            && ext != "sock"
            && ext != "console"
    }

    /// Reclaim the rootfs backings a reaped child left behind.
    ///
    /// A child killed while the controller is up has its backing removed by
    /// `Vm::drop`. This covers the other order — controller crash, then startup
    /// reap — where nothing else collects them: the watchdog leaves the dir
    /// alone because the socket is present, and the socket is present because
    /// the orphan held it until now. Only backings in a directory belonging to
    /// a process this reap has just confirmed dead are touched, so a live VM
    /// cannot be caught; the dir itself is left to the watchdog's own sweep.
    fn reclaim_child_backings(work_dir: &std::path::Path) {
        let Ok(entries) = std::fs::read_dir(work_dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if Self::is_child_backing(&name) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    pub(crate) fn kill_orphans(&self) -> Result<KillOrphansResult> {
        // One collected orphan: (sandbox id, pid, recorded start time,
        // recorded boot id).
        type OrphanRow = (String, u32, Option<u64>, Option<String>);

        // Single lock pass over retained rows (review #299 non-blocking:
        // fold the previous two acquisitions into one guard):
        //  - `unresolved`: rows with NO recorded PID. Such an entry is
        //    legacy/corrupt identity (every production registration path
        //    writes `Some(pid)`), but the absence of a PID is NOT evidence
        //    that no live VM is holding its netns/tap resources — on the
        //    contrary, it mirrors the #298 collision risk if we simply
        //    skip it and start with an empty allocator. The caller treats
        //    `unresolved > 0` as a startup blocker (fail closed).
        //  - `orphans`: rows with a live PID, collected with their
        //    recorded durable identity (start time + boot id). The lock
        //    is dropped before any kill so the (bounded) wait_for_death
        //    poll never holds the registry mutex.
        let (unresolved, orphans): (usize, Vec<OrphanRow>) = {
            let g = self.inner.lock();
            let mut unresolved = 0usize;
            let mut orphans = Vec::new();
            for (id, sb) in g.sandboxes.iter() {
                match sb.pid {
                    Some(pid) if pid_alive(pid) => {
                        orphans.push((id.clone(), pid, sb.proc_starttime, sb.boot_id.clone()))
                    }
                    Some(_) => {} // dead PID: reconcile() has already pruned these
                    None => unresolved += 1,
                }
            }
            (unresolved, orphans)
        };

        let mut killed = 0usize;
        let mut pruned_stale = 0usize;
        let mut kill_failed = 0usize;
        let mut legacy_unverified = 0usize;

        for (id, pid, recorded_starttime, recorded_boot_id) in orphans {
            // Primary identity check: compare the live process start time
            // (gated by the persisted boot id) against the record at
            // registration. This is the durable identity that survives
            // PID reuse — `comm` alone is spoofable and cannot distinguish
            // two Firecracker processes that happen to share a PID over
            // time (or across a host reboot).
            match process_identity_matches(pid, recorded_starttime, recorded_boot_id.as_deref()) {
                IdentityCheck::Match => {
                    // Same process (start time matches). Open a pidfd
                    // NOW, after the identity check, so the signal is
                    // pinned to the process that currently holds the PID.
                    // This narrows the TOCTOU window to the open→signal
                    // interval: even if the PID is reused between here and
                    // pidfd_send_signal, the fd targets whatever was pinned
                    // at open time, not whatever currently holds the PID.
                    //
                    // There is a residual window between the starttime
                    // read (in process_identity_matches) and pidfd_open:
                    // the original could exit and the PID could be recycled
                    // by another Firecracker in that gap. We close it by
                    // re-reading the start time AFTER pidfd_open succeeds and
                    // comparing again. Because pidfd pins the process at
                    // open time, /proc/<pid> reflects the pinned process;
                    // a mismatch means the PID was reused between the check
                    // and the open → prune without killing (PidReuse).
                    let pidfd = match pidfd_open(pid) {
                        Ok(fd) => fd,
                        Err(e) if e.raw_os_error() == Some(libc::ESRCH) => {
                            // Benign race: process exited between the
                            // identity check and pidfd_open. Safe to prune.
                            tracing::debug!(
                                sandbox_id = %id,
                                pid = pid,
                                "orphan exited before pidfd_open (ESRCH); pruning registry entry"
                            );
                            self.inner.lock().sandboxes.remove(&id);
                            pruned_stale += 1;
                            continue;
                        }
                        Err(e) => {
                            // pidfd_open can fail with EINVAL/ENOSYS on
                            // kernels < 5.3. Fail closed: keep the entry
                            // so the operator can investigate, rather than
                            // risk killing the wrong process via kill(2).
                            tracing::error!(
                                sandbox_id = %id,
                                pid = pid,
                                error = %e,
                                "pidfd_open failed; keeping registry entry to prevent unsafe kill"
                            );
                            kill_failed += 1;
                            continue;
                        }
                    };

                    // Re-verify the start time AFTER pidfd_open. This
                    // closes the check→open TOCTOU window: if the PID was
                    // reused between process_identity_matches and
                    // pidfd_open, the live start time now differs from
                    // the recorded one. (Note: /proc/<pid> reflects
                    // whoever CURRENTLY holds the PID — pidfd pinning
                    // does NOT change that. The re-read is what makes
                    // this safe: a mismatch proves the PID changed hands
                    // before we pinned it.) Prune without killing: the
                    // pinned process is not ours. Recorded_starttime is
                    // Some by construction here (Match requires it), but
                    // guard for clarity.
                    if let Some(recorded) = recorded_starttime {
                        match read_proc_starttime(pid) {
                            Some(live) if live == recorded => {
                                // pidfd-pinned process is the original.
                            }
                            _ => {
                                let _ = unsafe { libc::close(pidfd) };
                                tracing::warn!(
                                    sandbox_id = %id,
                                    pid = pid,
                                    "PID reused between identity check and pidfd_open \
                                     (start time changed); pruning without killing"
                                );
                                self.inner.lock().sandboxes.remove(&id);
                                pruned_stale += 1;
                                continue;
                            }
                        }
                    }

                    // Secondary confirmation: the comm name should still
                    // be firecracker. This is NOT a security boundary (comm
                    // is spoofable) — it catches corrupted state.json that
                    // somehow recorded a valid-looking start time for a
                    // non-firecracker process. If it fails, fail closed.
                    if !comm_is_firecracker(pid) {
                        let _ = unsafe { libc::close(pidfd) };
                        tracing::error!(
                            sandbox_id = %id,
                            pid = pid,
                            "start time matched but comm is not firecracker; \
                             keeping registry entry (possible state corruption)"
                        );
                        kill_failed += 1;
                        continue;
                    }

                    tracing::warn!(
                        sandbox_id = %id,
                        pid = pid,
                        "killing orphaned Firecracker process on startup (pidfd signal, identity verified)"
                    );
                    // Read the child's work dir now: once the process is gone
                    // there is nothing left in /proc to read it from, and the
                    // rootfs backing inside would be stranded for good.
                    let orphan_work_dir = Self::child_work_dir_from_cmdline(pid);
                    match pidfd_send_kill(pidfd) {
                        Ok(()) => {
                            // Wait for the process to actually exit (bounded).
                            // SIGKILL is asynchronous; a D-state process can
                            // hold netns/tap resources past the kill return.
                            if wait_for_death(pid, std::time::Duration::from_secs(5)) {
                                self.inner.lock().sandboxes.remove(&id);
                                if let Some(dir) = &orphan_work_dir {
                                    Self::reclaim_child_backings(dir);
                                }
                                killed += 1;
                            } else {
                                tracing::error!(
                                    sandbox_id = %id,
                                    pid = pid,
                                    "orphaned Firecracker did not exit within 5s of SIGKILL; \
                                     keeping registry entry to prevent resource collision"
                                );
                                kill_failed += 1;
                            }
                        }
                        Err(e) if e.raw_os_error() == Some(libc::ESRCH) => {
                            // Process exited between pidfd_open and the
                            // signal (benign). Safe to prune.
                            tracing::debug!(
                                sandbox_id = %id,
                                pid = pid,
                                "orphan exited before signal (ESRCH); pruning registry entry"
                            );
                            self.inner.lock().sandboxes.remove(&id);
                            pruned_stale += 1;
                        }
                        Err(e) => {
                            tracing::error!(
                                sandbox_id = %id,
                                pid = pid,
                                error = %e,
                                "pidfd_send_signal failed; keeping registry entry to prevent resource collision"
                            );
                            kill_failed += 1;
                        }
                    }
                    // Always close the pidfd; leak protection on every path.
                    let _ = unsafe { libc::close(pidfd) };
                }
                IdentityCheck::PidReuse => {
                    // PID is alive but the start time differs: the original
                    // Firecracker exited and the PID was recycled by the
                    // kernel for an unrelated process. Do NOT kill — prune
                    // the stale registry entry only.
                    tracing::warn!(
                        sandbox_id = %id,
                        pid = pid,
                        "PID reused (start time mismatch); pruning stale registry entry without killing"
                    );
                    self.inner.lock().sandboxes.remove(&id);
                    pruned_stale += 1;
                }
                IdentityCheck::Dead => {
                    // Process is gone (no /proc/<pid>). Prune the entry.
                    tracing::debug!(
                        sandbox_id = %id,
                        pid = pid,
                        "orphan already exited; pruning registry entry"
                    );
                    self.inner.lock().sandboxes.remove(&id);
                    pruned_stale += 1;
                }
                IdentityCheck::Unknown => {
                    // Identity genuinely unverifiable in THIS environment:
                    // the live boot id or start time could not be read, or
                    // off-Linux. Fail closed: do NOT kill (we can't prove
                    // the PID is ours), and do NOT silently prune (the
                    // entry may be for a legitimately-recoverable
                    // sandbox). Keep the entry so the operator can
                    // investigate; the startup abort check on
                    // kill_failed > 0 will surface this.
                    tracing::warn!(
                        sandbox_id = %id,
                        pid = pid,
                        recorded_starttime = ?recorded_starttime,
                        "cannot verify process identity (live boot id or start time \
                         unreadable, or off-Linux); keeping registry entry (fail closed)"
                    );
                    kill_failed += 1;
                }
                IdentityCheck::LegacyUnverifiable => {
                    // The registry row itself predates durable identity
                    // tracking (no recorded start time or boot id — serde
                    // defaults when an old state.json is read by this
                    // build). Fail closed, but with a distinct diagnostic
                    // and counter so the upgrade path is recognizable
                    // (review #299): the fix is to kill the PID manually
                    // (or remove the entry once verified dead) before
                    // restarting — not to debug the environment.
                    tracing::warn!(
                        sandbox_id = %id,
                        pid = pid,
                        "legacy registry row without recorded boot identity \
                         (state.json written by an older forkd); cannot verify \
                         the live PID is ours, keeping entry (fail closed). \
                         Kill the PID manually (kill {}) or remove the entry \
                         after verifying it is dead, then restart",
                        pid
                    );
                    legacy_unverified += 1;
                }
            }
        }

        let stale_ws = self.mark_stale_workspaces();
        if killed > 0 || pruned_stale > 0 || stale_ws {
            self.flush()?;
        }
        Ok(KillOrphansResult {
            killed,
            pruned_stale,
            kill_failed,
            unresolved,
            legacy_unverified,
        })
    }

    /// For metrics: live counts.
    pub fn counts(&self) -> (usize, usize) {
        let g = self.inner.lock();
        (g.snapshots.len(), g.sandboxes.len())
    }
}

/// Result of `kill_orphans`: how many were actually killed, pruned as
/// stale (PID reuse / already dead), how many kills failed, and how many
/// retained rows were unresolvable (no recorded PID).
#[derive(Debug, Default, Clone, Copy)]
pub struct KillOrphansResult {
    pub killed: usize,
    pub pruned_stale: usize,
    pub kill_failed: usize,
    /// Retained sandbox entries whose `pid` is `None`. This is a
    /// startup blocker: it means a previous controller left a row we
    /// cannot attribute to any live or dead process, so we cannot prove
    /// its netns/tap resources are free. Treating it as skippable would
    /// recreate the #298 collision risk (startup succeeds with an empty
    /// allocator while a live VM may still hold those resources).
    pub unresolved: usize,
    /// Retained sandbox entries whose row predates durable identity
    /// tracking (no recorded start time or boot id). Startup aborts on
    /// these too (fail closed), but with a distinct diagnostic: the
    /// operator-facing fix is manual verification/removal of legacy
    /// rows, not an environment investigation (review #299).
    pub legacy_unverified: usize,
}

#[cfg(target_os = "linux")]
fn pid_alive(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

#[cfg(not(target_os = "linux"))]
fn pid_alive(_pid: u32) -> bool {
    // Off-Linux (dev box on macOS / Windows): conservatively assume alive
    // so reconcile doesn't wipe state during local builds.
    true
}

// ----------------------------------------------------------------
// Durable process identity for orphan recovery (review #299 r6).
// ----------------------------------------------------------------
//
// `comm == "firecracker"` alone is NOT an identity check: `comm` is
// settable via `prctl(PR_SET_NAME)`, and a Firecracker that exits can
// have its PID recycled by *another* Firecracker before recovery runs.
// Killing on a comm match alone is a cross-tenant DoS vector and a
// same-name PID-reuse regression.
//
// The durable identity is the process start time (field 22 of
// `/proc/<pid>/stat`, in clock ticks since boot). It is captured at VM
// registration and persisted in `SandboxInfo::proc_starttime`. On
// controller restart, `process_identity_matches` compares the live
// start time against the recorded one. A mismatch means the recorded
// process has exited and the PID was reused — prune the stale entry,
// do NOT kill. Signalling through a pidfd opened AFTER the identity
// check narrows the TOCTOU window to the open→signal interval (the
// pidfd is pinned to the process that owned the PID at `pidfd_open`
// time, so a PID reuse after the open cannot redirect the signal).
// The residual check→open window is closed by RE-VERIFYING the
// start time after `pidfd_open` succeeds: if the live start time
// differs from the recorded one, the PID was reused between the
// check and the open, so we prune without killing. (Linux 5.3+;
// raw `libc::syscall` because libc 0.2.x exposes
// `SYS_pidfd_open`/`SYS_pidfd_send_signal` but not named fns.)
//
// Off-Linux stubs return identity-unknown so `kill_orphans` FAILS
// CLOSED (keeps the entry, increments kill_failed) rather than
// killing — the safe path for dev boxes, since killing an
// unidentifiable process is unsafe. (Not a prune: a prune would
// silently drop a possibly-live sandbox's state.)

/// Outcome of verifying a recorded PID's identity on recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdentityCheck {
    /// Live process start time matches the recorded one — same process,
    /// safe to signal through the pidfd.
    Match,
    /// PID is alive but the start time differs — the original process
    /// exited and the PID was reused. Prune the stale entry; do NOT kill.
    PidReuse,
    /// The process is gone (no `/proc/<pid>`). Prune the stale entry.
    Dead,
    /// Identity could not be verified (the live boot id or start time
    /// could not be read, or off-Linux). Fail closed:
    /// `kill_orphans` keeps the entry and increments `kill_failed`
    /// (aborting startup via the orphan identity gate) rather than
    /// risk killing an unidentifiable process.
    Unknown,
    /// The registry row predates durable identity tracking: it carries
    /// no recorded start time or boot id (serde defaults when an old
    /// `state.json` is read by a newer forkd). Fail closed with a
    /// DISTINCT diagnostic from `Unknown` (counted as
    /// `legacy_unverified`, review #299): the operator-facing cause and
    /// fix is "legacy row — kill the PID manually or remove the entry
    /// before upgrading", not an environment/EPERM debugging session.
    LegacyUnverifiable,
}

/// Read the Linux boot identity from `/proc/sys/kernel/random/boot_id`.
/// This is a UUID that changes on every host reboot.
///
/// `proc_starttime` (ticks since boot) is only meaningful *within* a single
/// boot: it is not unique across reboots. Two processes in different boots
/// can share both a numeric PID and a boot-relative start tick. Since the
/// registry persists across reboots (`/var/lib/forkd/state.json`), a
/// bare starttime is insufficient to prove a recorded PID is the same
/// still-alive Firecracker after the host has rebooted. We therefore also
/// persist the boot identity at registration and verify it on recovery
/// (review #299: cross-reboot PID+starttick reuse).
///
/// Returns `None` when the boot id cannot be read (unlikely on Linux; e.g.
/// no `/proc`, or a constrained container). Off-Linux this returns `None`.
#[cfg(target_os = "linux")]
pub(crate) fn read_boot_id() -> Option<String> {
    let raw = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    let boot_id = raw.trim();
    if boot_id.is_empty() {
        return None;
    }
    Some(boot_id.to_string())
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn read_boot_id() -> Option<String> {
    None
}

/// Read the boot id for sandbox REGISTRATION (review #299). Unlike
/// `read_boot_id`, a `None` here is treated as a spawn-blocking error
/// rather than recorded silently: a sandbox registered without a boot
/// id can never pass the startup identity check (its row would
/// permanently take the fail-closed path), leaving the controller
/// unable to start until state.json is hand-edited. Both production
/// registration paths (http.rs) call this BEFORE any VM work so a
/// degraded environment fails the spawn loudly instead of bricking
/// the next restart.
///
/// Off-Linux this intentionally returns `Ok(None)`: registration on
/// dev boxes must keep working (existing `#[cfg(not(target_os =
/// "linux"))]` fallbacks), and orphan recovery already reports
/// off-Linux rows via its own distinct diagnostics.
#[cfg(target_os = "linux")]
pub(crate) fn registration_boot_id() -> Result<Option<String>> {
    match read_boot_id() {
        Some(id) => Ok(Some(id)),
        None => Err(anyhow::anyhow!(
            "cannot read /proc/sys/kernel/random/boot_id; refusing to register a \
             sandbox without boot identity (its recovery could never be verified, \
             which would block every controller restart). Check that /proc is \
             mounted and the controller runs in an unrestricted environment"
        )),
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn registration_boot_id() -> Result<Option<String>> {
    Ok(None)
}

/// Read field 22 (starttime, clock ticks since boot) from
/// `/proc/<pid>/stat`. Returns `None` if the process is gone or the
/// file cannot be parsed.
///
/// The `comm` field (field 2) is enclosed in parentheses and may
/// itself contain spaces and parentheses, so naive whitespace
/// splitting is wrong. The robust parse: split at the LAST `)` in the
/// line, then tokenize the remainder; starttime is the 20th token
/// after the `)` (field 22 counting `pid` + `comm`).
#[cfg(target_os = "linux")]
pub(crate) fn read_proc_starttime(pid: u32) -> Option<u64> {
    if pid <= 1 {
        return None;
    }
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Field 2 (`comm`) is in parentheses and can contain spaces/parens.
    // Everything after the closing paren of comm is whitespace-separated.
    let after_comm = stat.rfind(')')?;
    let rest = &stat[after_comm + 1..];
    // rest begins with a space then: state(3) ppid(4) pgrp(5) ... starttime(22)
    // That is 20 fields after `)`: state, ppid, pgrp, session, tty_nr,
    // tpgid, flags, minflt, cminflt, majflt, cmajflt, utime, stime,
    // cutime, cstime, priority, nice, num_threads, itrealvalue,
    // starttime  → index 19 (0-based) in the whitespace-split of `rest`.
    let fields: Vec<&str> = rest.split_whitespace().collect();
    fields.get(19).and_then(|t| t.parse::<u64>().ok())
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn read_proc_starttime(_pid: u32) -> Option<u64> {
    None
}

/// Compare a recorded process identity against the live process at the
/// same PID. On Linux this compares (a) the boot identity and (b) the
/// start time; off-Linux it is always `Unknown` (no `/proc` to read).
///
/// `recorded_boot_id` is the persisted `/proc/sys/kernel/random/boot_id`
/// captured at registration. A mismatch with the current boot id means the
/// recorded process belonged to a previous boot, so it cannot still be
/// alive — we prune without signaling (`PidReuse`). A `None` recorded boot
/// id (legacy `state.json` written before the field existed, or a process
/// registered when the boot id was unreadable) means we cannot prove which
/// boot the process belongs to, so we FAIL CLOSED (`Unknown`) rather than
/// risk a cross-reboot false match (review #299).
#[cfg(target_os = "linux")]
fn process_identity_matches(
    pid: u32,
    recorded_starttime: Option<u64>,
    recorded_boot_id: Option<&str>,
) -> IdentityCheck {
    if pid <= 1 {
        return IdentityCheck::Unknown;
    }
    let Some(recorded) = recorded_starttime else {
        // No recorded start time: the row predates durable identity
        // tracking (old state.json, serde default). A DISTINCT outcome
        // from `Unknown` so the operator sees "legacy row" rather than
        // an environment problem (review #299). Fail closed.
        return IdentityCheck::LegacyUnverifiable;
    };
    // Boot identity gates the start-time comparison. If we cannot confirm
    // the recorded process was (or is) from the CURRENT boot, we must not
    // signal it: a cross-reboot PID+starttick collision would otherwise be
    // a false `Match` against an unrelated Firecracker.
    match recorded_boot_id {
        None => {
            // Missing recorded boot id: the row predates boot-id tracking
            // (serde default on old state.json). We cannot prove which
            // boot the process belongs to, so fail closed as a legacy
            // row (review #299).
            return IdentityCheck::LegacyUnverifiable;
        }
        Some(rec) => match read_boot_id() {
            None => {
                // Live boot id unreadable → fail closed (genuinely
                // unverifiable environment, not a legacy row).
                return IdentityCheck::Unknown;
            }
            Some(live) if live != rec => {
                // The recorded process is from a previous boot; it cannot
                // still be alive. Safe to prune without signaling.
                return IdentityCheck::PidReuse;
            }
            Some(_live_matching) => {
                // Same boot — proceed to the start-time check below.
            }
        },
    }
    let path = format!("/proc/{pid}");
    if !std::path::Path::new(&path).exists() {
        return IdentityCheck::Dead;
    }
    match read_proc_starttime(pid) {
        Some(live) if live == recorded => IdentityCheck::Match,
        Some(_live) => IdentityCheck::PidReuse,
        None => IdentityCheck::Unknown,
    }
}

#[cfg(not(target_os = "linux"))]
fn process_identity_matches(
    _pid: u32,
    _recorded_starttime: Option<u64>,
    _recorded_boot_id: Option<&str>,
) -> IdentityCheck {
    // Off-Linux there is no /proc to verify against, so every sandbox
    // row is unverifiable. Report LEGACY rows distinctly so a dev-box
    // startup abort names the real cause (review #299 non-blocking:
    // off-Linux builds fail with the same diagnostics).
    if _recorded_starttime.is_none() || _recorded_boot_id.is_none() {
        IdentityCheck::LegacyUnverifiable
    } else {
        IdentityCheck::Unknown
    }
}

/// Open a pidfd for a live process (Linux 5.3+). Used to close the
/// TOCTOU window between `process_identity_matches` and the signal:
/// the pidfd is pinned to the process that owned the PID at open time,
/// so a PID reuse between check and signal cannot redirect the kill.
///
/// Returns a raw fd on success, or an `io::Error` on failure. The
/// caller owns the fd and must close it (dropping is fine — it's a
/// plain fd, not a Rust-owned handle).
#[cfg(target_os = "linux")]
fn pidfd_open(pid: u32) -> std::io::Result<std::os::fd::RawFd> {
    // libc 0.2.x exposes the syscall numbers but not named functions.
    let ret = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as i32, 0u32) };
    if ret < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(ret as std::os::fd::RawFd)
    }
}

#[cfg(not(target_os = "linux"))]
fn pidfd_open(_pid: u32) -> std::io::Result<std::os::fd::RawFd> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "pidfd_open is Linux-only",
    ))
}

/// Send SIGKILL through a pidfd (Linux 5.3+). Unlike `kill(pid, ...)`,
/// this targets the process the pidfd was pinned to at `pidfd_open`
/// time, eliminating the PID-reuse TOCTOU window entirely.
#[cfg(target_os = "linux")]
fn pidfd_send_kill(pidfd: std::os::fd::RawFd) -> std::io::Result<()> {
    let ret = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd,
            libc::SIGKILL,
            std::ptr::null::<libc::siginfo_t>(),
            0u32,
        )
    };
    if ret < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(target_os = "linux"))]
fn pidfd_send_kill(_pidfd: std::os::fd::RawFd) -> std::io::Result<()> {
    Ok(())
}

/// Defensive `comm`-name check, used ONLY as a secondary confirmation
/// after the primary start-time identity check passes. This is NOT a
/// security boundary (`comm` is spoofable via `prctl`); it exists to
/// catch corrupted state.json that somehow records a valid-looking
/// start time for the wrong process. Uses exact `== "firecracker"`.
#[cfg(target_os = "linux")]
fn comm_is_firecracker(pid: u32) -> bool {
    if pid <= 1 {
        return false;
    }
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .map(|s| s.trim() == "firecracker")
        .unwrap_or(false)
}

#[cfg(not(target_os = "linux"))]
fn comm_is_firecracker(_pid: u32) -> bool {
    false
}

/// Poll for process death by checking `/proc/<pid>` disappearance.
/// Returns true if the process exited within the timeout, false if it
/// is still alive (e.g. stuck in D-state on I/O).
#[cfg(target_os = "linux")]
fn wait_for_death(pid: u32, timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    let path = format!("/proc/{pid}");
    while std::time::Instant::now() < deadline {
        if !std::path::Path::new(&path).exists() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    !std::path::Path::new(&path).exists()
}

#[cfg(not(target_os = "linux"))]
fn wait_for_death(_pid: u32, _timeout: std::time::Duration) -> bool {
    // Off-Linux: no /proc to poll; assume the process is gone.
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::SandboxInfo;
    use std::sync::{Arc, Barrier};
    use tempfile::TempDir;

    fn sandbox(id: impl Into<String>) -> SandboxInfo {
        SandboxInfo {
            id: id.into(),
            snapshot_tag: "py".into(),
            netns: Some("forkd-child-1".into()),
            guest_addr: "10.42.0.2:8888".into(),
            created_at_unix: 1,
            proc_starttime: None,
            // Recorded against the CURRENT boot so identity checks that
            // expect a match (same boot) pass; tests that need a
            // cross-boot mismatch set this explicitly.
            boot_id: read_boot_id(),
            pid: Some(99999999),
            memory_limit_mib: None,
            has_branched: false,
            last_branch_memory_path: None,
            branch_count: 0,
        }
    }

    fn running_workspace(
        name: impl Into<String>,
        live_sandbox_id: impl Into<String>,
    ) -> WorkspaceInfo {
        WorkspaceInfo {
            id: "ws-id-1".into(),
            name: name.into(),
            source_snapshot_tag: "py".into(),
            current_state_tag: None,
            status: WorkspaceStatus::Running,
            live_sandbox_id: Some(live_sandbox_id.into()),
            created_at_unix: 1,
            last_active_unix: 1,
            last_branch_memory_path: None,
            per_child_netns: false,
        }
    }

    #[test]
    fn reconcile_prunes_dead_sandbox_and_marks_its_workspace_stale() {
        let td = TempDir::new().unwrap();
        let path = td.path().join("state.json");
        let r = Registry::load_or_init(&path).unwrap();

        // "sb-dead" carries an unreachable pid (see `sandbox()`), so
        // reconcile must prune it; "ws-a" was Running on it, so it must
        // flip to Stale with live_sandbox_id cleared. "sb-live" has no
        // pid recorded at all (pid: None is never treated as dead), so
        // "ws-b" must be untouched, and Suspended "ws-c" must stay
        // Suspended even though its sandbox is also gone.
        r.insert_sandbox(sandbox("sb-dead")).unwrap();
        let mut sb_live = sandbox("sb-live");
        sb_live.pid = None;
        r.insert_sandbox(sb_live).unwrap();
        r.insert_workspace(running_workspace("ws-a", "sb-dead"))
            .unwrap();
        r.insert_workspace(running_workspace("ws-b", "sb-live"))
            .unwrap();
        r.insert_workspace(WorkspaceInfo {
            status: WorkspaceStatus::Suspended,
            live_sandbox_id: None,
            ..running_workspace("ws-c", "sb-gone-already")
        })
        .unwrap();

        let pruned = r.reconcile().unwrap();

        assert_eq!(pruned, 1, "only sb-dead has a recorded, unreachable pid");
        assert!(r.get_sandbox("sb-dead").is_none());
        assert!(r.get_sandbox("sb-live").is_some());

        let ws_a = r.get_workspace("ws-a").unwrap();
        assert_eq!(ws_a.status, WorkspaceStatus::Stale);
        assert_eq!(ws_a.live_sandbox_id, None);

        let ws_b = r.get_workspace("ws-b").unwrap();
        assert_eq!(ws_b.status, WorkspaceStatus::Running);
        assert_eq!(ws_b.live_sandbox_id.as_deref(), Some("sb-live"));

        let ws_c = r.get_workspace("ws-c").unwrap();
        assert_eq!(ws_c.status, WorkspaceStatus::Suspended);

        // Reloading from disk proves flush() actually ran (pruned > 0).
        let reloaded = Registry::load_or_init(&path).unwrap();
        assert_eq!(reloaded.list_sandboxes().len(), 1);
        assert_eq!(
            reloaded.get_workspace("ws-a").unwrap().status,
            WorkspaceStatus::Stale
        );
    }

    #[test]
    fn reconcile_no_op_does_not_flush() {
        let td = TempDir::new().unwrap();
        let path = td.path().join("state.json");
        let r = Registry::load_or_init(&path).unwrap();
        let mut sb_live = sandbox("sb-live");
        sb_live.pid = None;
        r.insert_sandbox(sb_live).unwrap();

        assert!(
            path.exists(),
            "insert_sandbox should have flushed once already"
        );
        let before = fs::read_to_string(&path).unwrap();

        let pruned = r.reconcile().unwrap();

        assert_eq!(pruned, 0);
        let after = fs::read_to_string(&path).unwrap();
        assert_eq!(
            before, after,
            "a no-op reconcile must not rewrite state.json"
        );
    }

    #[test]
    fn round_trip_persist_load() {
        let td = TempDir::new().unwrap();
        let path = td.path().join("state.json");

        let r = Registry::load_or_init(&path).unwrap();
        r.insert_sandbox(sandbox("sb-1")).unwrap();

        let r2 = Registry::load_or_init(&path).unwrap();
        assert_eq!(r2.list_sandboxes().len(), 1);
        assert_eq!(r2.list_sandboxes()[0].id, "sb-1");
    }

    #[test]
    fn concurrent_mutations_persist_without_tmp_file_races() {
        const WORKERS: usize = 20;
        const MUTATIONS_PER_WORKER: usize = 10;

        let td = TempDir::new().unwrap();
        let path = td.path().join("state.json");
        let registry = Registry::load_or_init(&path).unwrap();
        let barrier = Arc::new(Barrier::new(WORKERS));

        let workers: Vec<_> = (0..WORKERS)
            .map(|worker| {
                let registry = registry.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    for mutation in 0..MUTATIONS_PER_WORKER {
                        registry
                            .insert_sandbox(sandbox(format!("sb-{worker:02}-{mutation:02}")))?;
                    }
                    anyhow::Ok(())
                })
            })
            .collect();

        for worker in workers {
            worker.join().expect("worker thread panicked").unwrap();
        }

        let reloaded = Registry::load_or_init(&path).unwrap();
        assert_eq!(
            reloaded.list_sandboxes().len(),
            WORKERS * MUTATIONS_PER_WORKER
        );
    }

    /// The predicate decides what a startup reap deletes, and the work dir holds
    /// sockets and consoles under the same `child-<n>.` shape. The stem is the
    /// tag's rootfs filename, so it is not always `rootfs.ext4`.
    #[test]
    fn child_backing_predicate_matches_backings_only() {
        assert!(Registry::is_child_backing("child-1.rootfs.ext4"));
        assert!(Registry::is_child_backing("child-12.python-3-12-slim.ext4"));
        assert!(
            !Registry::is_child_backing("child-1.sock"),
            "sockets share the child-<n>. shape and must survive the sweep"
        );
        assert!(
            !Registry::is_child_backing("child-1.console"),
            "consoles share the child-<n>. shape and must survive the sweep"
        );
        assert!(!Registry::is_child_backing("memory-assembled.bin"));
        assert!(!Registry::is_child_backing("child.rootfs.ext4"));
        assert!(!Registry::is_child_backing("child-x.rootfs.ext4"));
        assert!(!Registry::is_child_backing("child-1."));
        assert!(!Registry::is_child_backing("rootfs.ext4"));
    }

    #[test]
    #[cfg(target_os = "linux")] // PidReuse path requires /proc starttime comparison
    fn kill_orphans_prunes_alive_pid_entries() {
        let td = TempDir::new().unwrap();
        let path = td.path().join("state.json");
        let r = Registry::load_or_init(&path).unwrap();

        // Insert a sandbox with an alive PID (our own process PID) but a
        // DELIBERATELY MISMATCHED start time. This simulates PID reuse:
        // the original Firecracker exited, the kernel recycled the PID
        // for an unrelated process (us), and the recorded start time no
        // longer matches. The durable-identity check must detect this
        // and prune the stale entry WITHOUT killing the unrelated process.
        //
        // The bogus start time (u64::MAX) cannot match any real process
        // (real start times are small clock-tick counts since boot).
        r.insert_sandbox(SandboxInfo {
            id: "sb-orphan".into(),
            snapshot_tag: "py".into(),
            netns: Some("forkd-child-1".into()),
            guest_addr: "10.42.0.2:8888".into(),
            created_at_unix: 1,
            pid: Some(std::process::id()),
            proc_starttime: Some(u64::MAX), // mismatched → PidReuse
            boot_id: read_boot_id(),
            memory_limit_mib: None,
            has_branched: false,
            last_branch_memory_path: None,
            branch_count: 0,
        })
        .unwrap();

        // Insert a sandbox with a dead PID (99999999 — not alive on Linux).
        // On macOS, pid_alive always returns true, so this entry survives
        // reconcile() and is pruned by kill_orphans() instead (as Dead on
        // Linux, or PidReuse via the mismatched start time on macOS).
        r.insert_sandbox(SandboxInfo {
            id: "sb-dead".into(),
            snapshot_tag: "py".into(),
            netns: Some("forkd-child-2".into()),
            guest_addr: "10.42.0.3:8888".into(),
            created_at_unix: 2,
            pid: Some(99999999),
            proc_starttime: Some(u64::MAX), // mismatched → PidReuse
            boot_id: read_boot_id(),
            memory_limit_mib: None,
            has_branched: false,
            last_branch_memory_path: None,
            branch_count: 0,
        })
        .unwrap();

        // reconcile prunes dead-PID entries (1 on Linux, 0 on macOS
        // where pid_alive always returns true).
        let _pruned = r.reconcile().unwrap();

        // kill_orphans prunes all remaining entries via the durable
        // identity check (start time mismatch → PidReuse → prune stale).
        let result = r.kill_orphans().unwrap();
        // At least one entry is pruned (the alive-PID one on Linux; both
        // on macOS where reconcile leaves both).
        assert!(result.killed + result.pruned_stale >= 1, "nothing pruned");
        // Nothing was killed — the mismatched start time means we never
        // believed the PID was our Firecracker, so we pruned, not killed.
        assert_eq!(result.killed, 0, "should not kill on identity mismatch");
        // All sandbox entries are gone.
        assert!(r.list_sandboxes().is_empty());
    }

    /// Regression (review #299): a recorded PID whose start time MATCHES
    /// the live process but whose recorded boot id differs from the
    /// current boot must NOT be killed.
    ///
    /// `proc_starttime` is ticks since boot and is only unique within one
    /// host boot. The registry persists across reboots, so after a host
    /// reboot an unrelated Firecracker can in principle share both the
    /// numeric PID and the boot-relative start tick with a recorded
    /// entry; a bare starttime check would then report `Match` and SIGKILL
    /// the wrong process. The boot id gate must turn that into a prune
    /// WITHOUT signaling: a different boot id means the recorded process
    /// (from the old boot) cannot still exist.
    ///
    /// We record the CURRENT real start time (so the starttime check would
    /// otherwise Match) but a recording of boot_id that differs from this
    /// boot (simulating a state.json carried across a reboot).
    #[test]
    #[cfg(target_os = "linux")] // needs a real process + /proc to prove Match-able identity
    fn kill_orphans_does_not_kill_across_boot_id_mismatch() {
        let td = TempDir::new().unwrap();
        let path = td.path().join("state.json");
        let r = Registry::load_or_init(&path).unwrap();

        // Spawn a real child (our own process will do — use the test's own
        // PID so it is alive; record its TRUE boot id-matching context but
        // store a DIFFERENT boot id in the registry). Using std::process::id()
        // keeps this deterministic and alive; the current boot id is read
        // fresh and a NONSENSE different boot id is persisted.
        let pid = std::process::id();
        let real_starttime = read_proc_starttime(pid).expect("read own starttime");
        // This boot's actual id, from a boot that does NOT match the
        // recorded value we are about to persist.
        let _current_boot = read_boot_id();

        // Persist an entry for OUR live PID with the REAL start time (so a
        // starttime-only check WOULD Match), but a boot id that is certainly
        // different from the current one. The boot gate must reject it
        // before the starttime check runs.
        r.insert_sandbox(SandboxInfo {
            id: "sb-cross-boot".into(),
            snapshot_tag: "py".into(),
            netns: Some("forkd-child-1".into()),
            guest_addr: "10.42.0.2:8888".into(),
            created_at_unix: 1,
            pid: Some(pid),
            proc_starttime: Some(real_starttime),
            // Guaranteed-different boot id (UUIDs are unique across boots;
            // a fresh random-looking value cannot equal the current boot's).
            boot_id: Some("00000000-0000-0000-0000-000000000000".to_string()),
            memory_limit_mib: None,
            has_branched: false,
            last_branch_memory_path: None,
            branch_count: 0,
        })
        .unwrap();

        let result = r.kill_orphans().unwrap();
        // The boot gate turns a would-be Match into a PidReuse → prune
        // WITHOUT signaling (killed stays 0).
        assert_eq!(
            result.killed, 0,
            "must NOT kill a process whose recorded boot id differs from current boot"
        );
        assert_eq!(
            result.pruned_stale, 1,
            "cross-boot stale entry must be pruned without signaling"
        );
        assert_eq!(result.kill_failed, 0);
        assert!(r.list_sandboxes().is_empty());
    }

    /// Upgrade-path contract (review #299): an alive-PID entry with NO
    /// recorded start time (state.json written before identity tracking
    /// existed) must FAIL CLOSED and be counted as `legacy_unverified`,
    /// not `kill_failed` — the operator-facing cause and fix is "legacy
    /// row, verify/kill the PID manually", which must be distinguishable
    /// from a genuine kill failure. Cross-platform contract test (the
    /// off-Linux stub reports legacy rows identically).
    #[test]
    fn kill_orphans_fails_closed_on_unknown_identity() {
        let td = TempDir::new().unwrap();
        let path = td.path().join("state.json");
        let r = Registry::load_or_init(&path).unwrap();

        // Alive PID (our own), no recorded start time → Unknown → fail closed.
        r.insert_sandbox(SandboxInfo {
            id: "sb-unknown".into(),
            snapshot_tag: "py".into(),
            netns: Some("forkd-child-1".into()),
            guest_addr: "10.42.0.2:8888".into(),
            created_at_unix: 1,
            pid: Some(std::process::id()),
            proc_starttime: None, // old state.json — pre-identity-tracking row
            // Legacy entry with no boot id — we cannot verify which boot
            // it belongs to, so the kill path fails closed as
            // LegacyUnverifiable (review #299).
            boot_id: None,
            memory_limit_mib: None,
            has_branched: false,
            last_branch_memory_path: None,
            branch_count: 0,
        })
        .unwrap();

        let result = r.kill_orphans().unwrap();
        // Fail closed: nothing killed, nothing pruned, the legacy counter
        // is incremented (distinct from a genuine kill failure).
        assert_eq!(result.killed, 0, "must not kill with unknown identity");
        assert_eq!(
            result.pruned_stale, 0,
            "must not prune with unknown identity"
        );
        assert_eq!(
            result.legacy_unverified, 1,
            "a pre-boot-id row must be counted as legacy_unverified (review #299)"
        );
        assert_eq!(
            result.kill_failed, 0,
            "legacy rows are counted separately from genuine kill failures"
        );
        // The entry is KEPT so the operator can investigate.
        assert_eq!(
            r.list_sandboxes().len(),
            1,
            "entry must be retained on unknown identity"
        );
    }

    /// Regression for the same-name Firecracker PID-reuse attack the
    /// reviewer flagged: a Firecracker exits, the kernel recycles its PID
    /// for ANOTHER Firecracker (same comm name), and the recorded start
    /// time no longer matches. The old comm-only check would kill the
    /// legitimate new Firecracker; the durable-identity check must detect
    /// the start-time mismatch and prune the stale entry WITHOUT killing.
    ///
    /// We simulate this by recording our own PID (alive) with a bogus
    /// start time. On Linux `comm_is_firecracker` would return false for
    /// us, but the PRIMARY check (start time) fires first and returns
    /// PidReuse before comm is ever consulted — so the entry is pruned as
    /// stale regardless of comm. This is the same-name reuse regression:
    /// the test fails if kill_orphans ever relies on comm alone.
    #[test]
    #[cfg(target_os = "linux")] // PidReuse path requires /proc starttime comparison
    fn kill_orphans_prunes_same_name_pid_reuse_without_killing() {
        let td = TempDir::new().unwrap();
        let path = td.path().join("state.json");
        let r = Registry::load_or_init(&path).unwrap();

        r.insert_sandbox(SandboxInfo {
            id: "sb-reused".into(),
            snapshot_tag: "py".into(),
            netns: Some("forkd-child-1".into()),
            guest_addr: "10.42.0.2:8888".into(),
            created_at_unix: 1,
            pid: Some(std::process::id()),
            proc_starttime: Some(0), // real start time is > 0; 0 cannot match
            boot_id: read_boot_id(),
            memory_limit_mib: None,
            has_branched: false,
            last_branch_memory_path: None,
            branch_count: 0,
        })
        .unwrap();

        let result = r.kill_orphans().unwrap();
        // The start-time mismatch (0 vs the real start time) must yield
        // PidReuse → pruned_stale, NOT killed. This is the crux: even if
        // comm were "firecracker", the mismatched start time prevents
        // the kill.
        assert_eq!(result.killed, 0, "must not kill on start-time mismatch");
        assert!(result.pruned_stale >= 1, "must prune stale on PID reuse");
        assert!(r.list_sandboxes().is_empty());
    }

    #[test]
    #[cfg(target_os = "linux")] // PidReuse path requires /proc starttime comparison
    fn kill_orphans_marks_workspaces_stale() {
        let td = TempDir::new().unwrap();
        let path = td.path().join("state.json");
        let r = Registry::load_or_init(&path).unwrap();

        // Insert a sandbox with an alive PID and a MISMATCHED start time
        // so the durable-identity check yields PidReuse → pruned (not killed).
        r.insert_sandbox(SandboxInfo {
            id: "sb-1".into(),
            snapshot_tag: "py".into(),
            netns: Some("forkd-child-1".into()),
            guest_addr: "10.42.0.2:8888".into(),
            created_at_unix: 1,
            pid: Some(std::process::id()),
            proc_starttime: Some(u64::MAX), // mismatched → PidReuse
            boot_id: read_boot_id(),
            memory_limit_mib: None,
            has_branched: false,
            last_branch_memory_path: None,
            branch_count: 0,
        })
        .unwrap();

        // Insert a workspace that references this sandbox.
        r.insert_workspace(WorkspaceInfo {
            id: "ws-1".into(),
            name: "ws-1".into(),
            source_snapshot_tag: "py".into(),
            current_state_tag: None,
            status: WorkspaceStatus::Running,
            live_sandbox_id: Some("sb-1".into()),
            created_at_unix: 1,
            last_active_unix: 1,
            last_branch_memory_path: None,
            per_child_netns: false,
        })
        .unwrap();

        // kill_orphans prunes the stale sandbox (PID reuse) and marks the
        // workspace Stale. Nothing is killed (identity mismatch).
        let result = r.kill_orphans().unwrap();
        assert_eq!(result.killed, 0); // identity mismatch → not killed
        assert_eq!(result.pruned_stale, 1); // pruned as stale
        assert!(r.list_sandboxes().is_empty());

        let ws = r.get_workspace("ws-1").unwrap();
        assert_eq!(ws.status, WorkspaceStatus::Stale);
        assert!(ws.live_sandbox_id.is_none());
    }

    #[test]
    fn kill_orphans_no_op_on_empty_registry() {
        let td = TempDir::new().unwrap();
        let path = td.path().join("state.json");
        let r = Registry::load_or_init(&path).unwrap();
        let result = r.kill_orphans().unwrap();
        assert_eq!(result.killed, 0);
        assert_eq!(result.pruned_stale, 0);
        assert_eq!(result.kill_failed, 0);
    }

    #[test]
    fn kill_orphans_counts_pid_none_entries_as_unresolved() {
        let td = TempDir::new().unwrap();
        let path = td.path().join("state.json");
        let r = Registry::load_or_init(&path).unwrap();

        // A sandbox row with pid: None is legacy/corrupt identity — every
        // production registration path writes Some(pid). The absence of a
        // PID is NOT evidence that no live VM holds the netns/tap it
        // registered, so rather than silently skip it (which would recreate
        // the #298 collision risk by starting with an empty allocator), it
        // must be surfaced as `unresolved` so the caller blocks startup.
        // Use a resource-holding shape (netns: Some(...)) to mirror the
        // real #298 collision this protects against: the pid-less row's
        // netns/tap may still be held by a live VM even though we can't
        // attribute it to any PID.
        r.insert_sandbox(SandboxInfo {
            id: "sb-no-pid".into(),
            snapshot_tag: "py".into(),
            netns: Some("forkd-child-1".into()),
            guest_addr: "10.42.0.2:8888".into(),
            created_at_unix: 1,
            proc_starttime: None,
            boot_id: None,
            pid: None,
            memory_limit_mib: None,
            has_branched: false,
            last_branch_memory_path: None,
            branch_count: 0,
        })
        .unwrap();

        let _ = r.reconcile().unwrap();
        let result = r.kill_orphans().unwrap();
        assert_eq!(result.killed, 0);
        assert_eq!(result.pruned_stale, 0);
        assert_eq!(result.kill_failed, 0);
        // The pid:None row is surfaced as unresolved → startup blocker.
        assert_eq!(
            result.unresolved, 1,
            "pid:None entry must be reported as unresolved, not skipped"
        );
        // Entry retained (not pruned) so the operator can inspect it.
        assert_eq!(r.list_sandboxes().len(), 1);

        // And the abort decision must fail closed on it (cross-platform
        // decision function; the Linux gate turns Some into Err).
        assert!(crate::orphan_abort_reason(&result).is_some());
    }

    #[test]
    #[cfg(target_os = "linux")] // PidReuse path requires /proc starttime comparison
    fn kill_orphans_marks_multiple_workspaces_stale() {
        let td = TempDir::new().unwrap();
        let path = td.path().join("state.json");
        let r = Registry::load_or_init(&path).unwrap();

        // Insert a sandbox with an alive PID and a MISMATCHED start time
        // so the durable-identity check yields PidReuse → pruned (not killed).
        r.insert_sandbox(SandboxInfo {
            id: "sb-1".into(),
            snapshot_tag: "py".into(),
            netns: Some("forkd-child-1".into()),
            guest_addr: "10.42.0.2:8888".into(),
            created_at_unix: 1,
            pid: Some(std::process::id()),
            proc_starttime: Some(u64::MAX), // mismatched → PidReuse
            boot_id: read_boot_id(),
            memory_limit_mib: None,
            has_branched: false,
            last_branch_memory_path: None,
            branch_count: 0,
        })
        .unwrap();

        // Insert two workspaces referencing the same sandbox.
        for name in ["ws-a", "ws-b"] {
            r.insert_workspace(WorkspaceInfo {
                id: name.into(),
                name: name.into(),
                source_snapshot_tag: "py".into(),
                current_state_tag: None,
                status: WorkspaceStatus::Running,
                live_sandbox_id: Some("sb-1".into()),
                created_at_unix: 1,
                last_active_unix: 1,
                last_branch_memory_path: None,
                per_child_netns: false,
            })
            .unwrap();
        }

        let result = r.kill_orphans().unwrap();
        assert_eq!(result.pruned_stale, 1);

        // Both workspaces should be marked Stale.
        assert_eq!(
            r.get_workspace("ws-a").unwrap().status,
            WorkspaceStatus::Stale
        );
        assert_eq!(
            r.get_workspace("ws-b").unwrap().status,
            WorkspaceStatus::Stale
        );
    }

    #[test]
    #[cfg(target_os = "linux")] // PidReuse path requires /proc starttime comparison
    fn kill_orphans_persists_to_disk() {
        let td = TempDir::new().unwrap();
        let path = td.path().join("state.json");
        let r = Registry::load_or_init(&path).unwrap();

        r.insert_sandbox(SandboxInfo {
            id: "sb-1".into(),
            snapshot_tag: "py".into(),
            netns: Some("forkd-child-1".into()),
            guest_addr: "10.42.0.2:8888".into(),
            created_at_unix: 1,
            pid: Some(std::process::id()),
            proc_starttime: Some(u64::MAX), // mismatched → PidReuse
            boot_id: read_boot_id(),
            memory_limit_mib: None,
            has_branched: false,
            last_branch_memory_path: None,
            branch_count: 0,
        })
        .unwrap();

        let result = r.kill_orphans().unwrap();
        assert!(result.pruned_stale >= 1);

        // Reload from disk and verify the entry is gone.
        let r2 = Registry::load_or_init(&path).unwrap();
        assert!(r2.list_sandboxes().is_empty());
    }

    #[test]
    #[cfg(target_os = "linux")] // PidReuse path requires /proc starttime comparison
    fn kill_orphans_reconcile_then_kill_integration() {
        let td = TempDir::new().unwrap();
        let path = td.path().join("state.json");
        let r = Registry::load_or_init(&path).unwrap();

        // Dead PID entry (pruned by reconcile).
        r.insert_sandbox(SandboxInfo {
            id: "sb-dead".into(),
            snapshot_tag: "py".into(),
            netns: Some("forkd-child-1".into()),
            guest_addr: "10.42.0.2:8888".into(),
            created_at_unix: 1,
            proc_starttime: Some(u64::MAX), // mismatched → PidReuse if it reaches kill_orphans
            boot_id: read_boot_id(),
            pid: Some(99999999),
            memory_limit_mib: None,
            has_branched: false,
            last_branch_memory_path: None,
            branch_count: 0,
        })
        .unwrap();

        // Alive PID entry (pruned by kill_orphans via PidReuse).
        r.insert_sandbox(SandboxInfo {
            id: "sb-alive".into(),
            snapshot_tag: "py".into(),
            netns: Some("forkd-child-2".into()),
            guest_addr: "10.42.0.3:8888".into(),
            created_at_unix: 2,
            pid: Some(std::process::id()),
            proc_starttime: Some(u64::MAX), // mismatched → PidReuse
            boot_id: read_boot_id(),
            memory_limit_mib: None,
            has_branched: false,
            last_branch_memory_path: None,
            branch_count: 0,
        })
        .unwrap();

        let pruned = r.reconcile().unwrap();
        let result = r.kill_orphans().unwrap();

        // On Linux: reconcile prunes sb-dead (1), kill_orphans prunes sb-alive (1).
        // On macOS: reconcile prunes 0 (all PIDs "alive"), kill_orphans prunes 2.
        assert!(r.list_sandboxes().is_empty());
        assert!(pruned + result.killed + result.pruned_stale >= 2);
    }

    /// Regression: kill_failed > 0 must cause the caller to abort startup,
    /// not silently continue with an empty NetnsAllocator. This test
    /// verifies the KillOrphansResult contract: on non-Linux platforms
    /// (where pid_is_firecracker returns false), alive-PID entries are
    /// pruned as stale (kill_failed stays 0). On Linux, a real
    /// firecracker PID that can't be killed would set kill_failed > 0.
    /// The caller (run_daemon) checks `if orphans.kill_failed > 0 {
    /// anyhow::bail!(...) }` — this test verifies that a result with
    /// kill_failed == 0 does NOT trigger the abort (baseline), and
    /// documents the contract.
    #[test]
    fn kill_orphans_result_kill_failed_contract() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.json");
        let r = Registry::load_or_init(path).unwrap();

        // No sandboxes — kill_orphans returns all-zero result.
        let result = r.kill_orphans().unwrap();
        assert_eq!(
            result.kill_failed, 0,
            "no sandboxes should yield 0 kill_failed"
        );
        assert_eq!(result.killed, 0);
        assert_eq!(result.pruned_stale, 0);

        // The caller's abort condition: kill_failed > 0 → bail.
        // With 0 kill_failed, startup should NOT abort (baseline).
        assert!(result.kill_failed == 0, "baseline should not abort");
    }

    /// Regression: check_orphan_kill_result must return Err when
    /// kill_failed > 0, proving that run_daemon aborts startup and
    /// no conflicting spawns can be admitted. This is the actual
    /// abort-decision test — the previous test only verified the
    /// zero-baseline contract.
    #[test]
    fn check_orphan_kill_result_aborts_on_kill_failure() {
        use crate::orphan_abort_reason;

        // kill_failed > 0 must abort (EPERM, D-state timeout, etc.)
        let result = KillOrphansResult {
            killed: 2,
            pruned_stale: 0,
            kill_failed: 1,
            unresolved: 0,
            legacy_unverified: 0,
        };
        let outcome = orphan_abort_reason(&result);
        assert!(
            outcome.is_some(),
            "kill_failed=1 must cause startup abort, got {outcome:?}"
        );
        let err = outcome.expect("expected an abort reason");
        assert!(
            err.contains("aborting startup"),
            "error should mention aborting startup, got: {err}"
        );
        assert!(
            err.contains('1'),
            "error should contain kill_failed count, got: {err}"
        );

        // kill_failed == 0 must NOT abort, even with killed/pruned entries
        let result = KillOrphansResult {
            killed: 5,
            pruned_stale: 3,
            kill_failed: 0,
            unresolved: 0,
            legacy_unverified: 0,
        };
        assert!(
            orphan_abort_reason(&result).is_none(),
            "kill_failed=0 should not abort even with killed/pruned entries"
        );
    }

    /// Regression: check_orphan_kill_result error message must
    /// contain the exact kill_failed count so operators can identify
    /// how many orphans need manual intervention.
    #[test]
    fn check_orphan_kill_result_error_contains_count() {
        use crate::orphan_abort_reason;

        let result = KillOrphansResult {
            killed: 0,
            pruned_stale: 0,
            kill_failed: 3,
            unresolved: 0,
            legacy_unverified: 0,
        };
        let err = orphan_abort_reason(&result).expect("kill_failed>0 must abort");
        assert!(
            err.contains('3'),
            "error should contain kill_failed count (3), got: {err}"
        );
        assert!(
            err.contains("could not be killed"),
            "error should explain the failure, got: {err}"
        );
    }

    /// Startup-decision regression (review #299): a retained sandbox row
    /// with no PID must block startup. Previously `kill_orphans` skipped
    /// `pid: None` entries and `check_orphan_kill_result` only looked at
    /// `kill_failed`, so the controller started with an empty allocator/
    /// shared-tap ownership while a live VM might still hold those
    /// resources — recreating the #298 collision risk.
    #[test]
    fn check_orphan_kill_result_aborts_on_unresolved() {
        use crate::orphan_abort_reason;

        // unresolved > 0 (even with kill_failed == 0) must abort.
        let result = KillOrphansResult {
            killed: 0,
            pruned_stale: 0,
            kill_failed: 0,
            unresolved: 2,
            legacy_unverified: 0,
        };
        let outcome = orphan_abort_reason(&result);
        assert!(
            outcome.is_some(),
            "unresolved=2 must cause startup abort, got {outcome:?}"
        );
        let err = outcome.expect("expected an abort reason");
        assert!(
            err.contains("aborting startup"),
            "error should mention aborting startup, got: {err}"
        );
        assert!(
            err.contains('2'),
            "error should contain unresolved count (2), got: {err}"
        );

        // unresolved == 0 and kill_failed == 0 must NOT abort.
        let ok = KillOrphansResult {
            killed: 1,
            pruned_stale: 0,
            kill_failed: 0,
            unresolved: 0,
            legacy_unverified: 0,
        };
        assert!(
            orphan_abort_reason(&ok).is_none(),
            "clean result should not abort startup"
        );
    }

    /// Linux-only: the gate wiring itself — `check_orphan_kill_result`
    /// must turn `orphan_abort_reason`'s Some into Err. (CI runs on Linux,
    /// so this is the actually-executed startup gate in production.)
    #[test]
    #[cfg(target_os = "linux")]
    fn check_orphan_kill_result_gate_wiring_on_linux() {
        let abort = KillOrphansResult {
            killed: 0,
            pruned_stale: 0,
            kill_failed: 0,
            unresolved: 0,
            legacy_unverified: 1,
        };
        assert!(
            crate::check_orphan_kill_result(&abort).is_err(),
            "legacy_unverified must abort on Linux"
        );
        let clean = KillOrphansResult {
            killed: 3,
            pruned_stale: 1,
            kill_failed: 0,
            unresolved: 0,
            legacy_unverified: 0,
        };
        assert!(
            crate::check_orphan_kill_result(&clean).is_ok(),
            "clean result must not abort on Linux"
        );
    }

    /// Upgrade-path regression (review #299): rows written by an older
    /// forkd carry no boot identity, so the first restart after upgrading
    /// fail-closes on them. The abort must be DISTINCT from a genuine
    /// kill failure (EPERM/timeout) so the operator sees "legacy row —
    /// verify/kill the PID manually" rather than an environment
    /// debugging session.
    #[test]
    fn check_orphan_kill_result_aborts_on_legacy_unverified() {
        use crate::orphan_abort_reason;

        // legacy_unverified > 0 (even with kill_failed == 0) must abort.
        let result = KillOrphansResult {
            killed: 0,
            pruned_stale: 0,
            kill_failed: 0,
            unresolved: 0,
            legacy_unverified: 1,
        };
        let outcome = orphan_abort_reason(&result);
        assert!(
            outcome.is_some(),
            "legacy_unverified=1 must cause startup abort, got {outcome:?}"
        );
        let err = outcome.expect("expected an abort reason");
        assert!(
            err.contains("aborting startup"),
            "error should mention aborting startup, got: {err}"
        );
        assert!(
            err.contains("older forkd"),
            "error should name the legacy-row cause, got: {err}"
        );
        assert!(
            err.contains('1'),
            "error should contain the legacy_unverified count, got: {err}"
        );
        // A genuine kill failure and a legacy row produce DIFFERENT
        // diagnostics — the legacy message must not mention EPERM.
        let killfail = KillOrphansResult {
            killed: 0,
            pruned_stale: 0,
            kill_failed: 1,
            unresolved: 0,
            legacy_unverified: 0,
        };
        let err = orphan_abort_reason(&killfail).expect("kill_failed>0 must abort");
        assert!(
            err.contains("could not be killed") && !err.contains("older forkd"),
            "kill-failure diagnostic must stay distinct from the legacy one, got: {err}"
        );
    }

    // ----------------------------------------------------------------
    // ce-code-review r6 follow-ups: Match-path coverage, starttime parser
    // unit test, and backward-compat serde default for proc_starttime.
    // ----------------------------------------------------------------

    /// Backward-compat: an old `state.json` written before `proc_starttime`
    /// existed must still deserialize (the field is `#[serde(default)]`),
    /// yielding `proc_starttime: None`. This is the contract that lets
    /// existing deployments upgrade without wiping state. Cross-platform
    /// (no /proc required).
    #[test]
    fn proc_starttime_defaults_to_none_when_absent_in_old_state_json() {
        let td = TempDir::new().unwrap();
        let path = td.path().join("state.json");
        // Hand-written old-format JSON: every SandboxInfo field EXCEPT
        // proc_starttime (simulating a pre-r6 state.json).
        let old_json = r#"{
          "sandboxes": {
            "sb-legacy": {
              "id": "sb-legacy",
              "snapshot_tag": "py",
              "netns": "forkd-child-1",
              "guest_addr": "10.42.0.2:8888",
              "created_at_unix": 1,
              "pid": 4242,
              "memory_limit_mib": null,
              "has_branched": false,
              "last_branch_memory_path": null,
              "branch_count": 0
            }
          },
          "workspaces": {}
        }"#;
        std::fs::write(&path, old_json).unwrap();

        let r = Registry::load_or_init(&path).unwrap();
        let sbs = r.list_sandboxes();
        assert_eq!(sbs.len(), 1, "legacy entry must load");
        assert_eq!(sbs[0].id, "sb-legacy");
        assert_eq!(
            sbs[0].proc_starttime, None,
            "proc_starttime must default to None when absent (backward compat)"
        );
    }

    /// Unit test for `read_proc_starttime` field-22 parsing. Covers the
    /// parenthesized-comm edge case (comm containing parens/spaces) that
    /// naive whitespace splitting gets wrong. Linux-only (needs /proc).
    #[test]
    #[cfg(target_os = "linux")]
    fn read_proc_starttime_parses_our_own_stat() {
        // Our own PID's starttime must be parseable and > 0 (clock ticks
        // since boot; the boot-relative value is always positive while
        // the system is up).
        let own = std::process::id();
        let st = read_proc_starttime(own);
        assert!(st.is_some(), "could not read /proc/{own}/stat starttime");
        assert!(
            st.unwrap() > 0,
            "starttime should be > 0 while system is up"
        );

        // PID 1 (init) is deliberately rejected by the parser guard.
        assert_eq!(read_proc_starttime(1), None, "pid <= 1 must be rejected");

        // A non-existent PID returns None.
        assert_eq!(
            read_proc_starttime(99_999_999),
            None,
            "nonexistent PID must return None"
        );
    }

    /// The secondary `comm_is_firecracker` check inside the `Match` arm
    /// is a defense-in-depth guard against corrupted `state.json` that
    /// somehow recorded a valid-looking start time for a non-firecracker
    /// process. This test exercises that fail-closed branch: it spawns a
    /// real child (`sleep`, whose comm is "sleep"), records its TRUE start
    /// time so `process_identity_matches` returns `Match`, inserts a
    /// sandbox entry, and asserts `kill_orphans` does NOT kill the
    /// non-firecracker process — it keeps the entry and increments
    /// `kill_failed`. Linux-only (pidfd_open + /proc).
    #[test]
    #[cfg(target_os = "linux")]
    fn kill_orphans_match_arm_comm_mismatch_fails_closed() {
        use std::process::{Command, Stdio};
        use std::time::Duration;

        let td = TempDir::new().unwrap();
        let path = td.path().join("state.json");
        let r = Registry::load_or_init(&path).unwrap();

        // Spawn a child whose comm is "sleep" (NOT "firecracker").
        // Its TRUE start time is recorded so `process_identity_matches`
        // returns `Match` and the pidfd path is entered, but the
        // secondary `comm_is_firecracker` check then fails closed —
        // proving a matching start time alone does NOT authorize a kill.
        let mut child = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sleep");
        let pid = child.id();

        // Record the child's REAL start time so process_identity_matches
        // returns Match (the primary identity check passes).
        let starttime = read_proc_starttime(pid).expect("read child starttime");

        r.insert_sandbox(SandboxInfo {
            id: "sb-match-child".into(),
            snapshot_tag: "py".into(),
            netns: Some("forkd-child-1".into()),
            guest_addr: "10.42.0.2:8888".into(),
            created_at_unix: 1,
            pid: Some(pid),
            proc_starttime: Some(starttime),
            boot_id: read_boot_id(),
            memory_limit_mib: None,
            has_branched: false,
            last_branch_memory_path: None,
            branch_count: 0,
        })
        .unwrap();

        let result = r.kill_orphans().unwrap();
        // The start time matched (Match) and pidfd_open + re-verification
        // succeeded, but comm_is_firecracker("sleep") is false → fail
        // closed: nothing killed, nothing pruned, kill_failed incremented,
        // entry retained. This proves the Match arm was entered and the
        // comm gate works as a secondary defense.
        assert_eq!(result.killed, 0, "must not kill a non-firecracker process");
        assert_eq!(result.pruned_stale, 0, "must not prune on comm mismatch");
        assert_eq!(
            result.kill_failed, 1,
            "Match arm reached but comm check failed closed (kill_failed)"
        );
        assert_eq!(
            r.list_sandboxes().len(),
            1,
            "entry must be retained on comm-mismatch fail-closed"
        );

        // Clean up the still-alive child.
        let _ = child.kill();
        let _ = child.wait_with_output();
        // Give the kernel a moment to reap so wait_for_death-style polls
        // see the process as gone.
        std::thread::sleep(Duration::from_millis(100));
    }

    /// The `Match` kill lifecycle (`pidfd_open` → start-time re-verification →
    /// `comm_is_firecracker` → `pidfd_send_kill` → `wait_for_death` →
    /// registry removal) had zero coverage — every other test used a
    /// mismatched/absent start time or a non-firecracker comm so the
    /// irreversible success path was never executed. This test spawns a
    /// disposable process whose `/proc/<pid>/comm` is literally
    /// `firecracker` (a copy of `sleep` renamed to `firecracker`, so the
    /// binary runs but reports the expected comm), records its TRUE start
    /// time, inserts a sandbox entry, and asserts `kill_orphans` actually
    /// kills it via the pidfd path: `killed == 1`, `kill_failed == 0`, and
    /// the registry entry is removed. Linux-only (pidfd_open + /proc).
    ///
    /// The child is **double-forked via a shell** so it is reparented to
    /// init (PID 1), not held as a child of this test process. This mirrors
    /// production: real Firecracker orphans are NOT children of the
    /// controller, so when `kill_orphans` SIGKILLs them, init reaps them
    /// and `/proc/<pid>` disappears quickly. A child of the test process
    /// would instead linger as a zombie (held by the test until reaped),
    /// causing `wait_for_death` to time out and report `kill_failed`.
    #[test]
    #[cfg(target_os = "linux")]
    fn kill_orphans_match_arm_kills_verified_firecracker_via_pidfd() {
        use std::process::{Command, Stdio};

        let td = TempDir::new().unwrap();
        let path = td.path().join("state.json");
        let r = Registry::load_or_init(&path).unwrap();

        // Create a disposable binary whose /proc/<pid>/comm is "firecracker".
        // /proc/<pid>/comm is derived from the executable's basename
        // (truncated to 15 chars), so a copy of `sleep` named `firecracker`
        // reports comm == "firecracker" while running the real `sleep` binary.
        let firecracker_bin = td.path().join("firecracker");
        std::fs::copy(
            std::env::var("FORKD_TEST_SLEEP_BIN")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| {
                    // /bin/sleep and /usr/bin/sleep are the usual locations;
                    // resolve via `which`-like lookup against common dirs.
                    ["/bin/sleep", "/usr/bin/sleep"]
                        .into_iter()
                        .map(std::path::PathBuf::from)
                        .find(|p| p.exists())
                        .expect("sleep binary not found in /bin/sleep or /usr/bin/sleep")
                }),
            &firecracker_bin,
        )
        .expect("copy sleep → firecracker");

        // Spawn the firecracker-named process as a DIRECT child held by this
        // test, NOT a detached/orphaned process. A detached, shell-backgrounded
        // child reparented to init is reaped too early by the CI runner (it
        // exits and becomes a zombie before kill_orphans runs), which makes
        // process_identity_matches return Dead and the test flaky. Owning the
        // Child keeps it alive until we choose to act.
        //
        // A private reaper thread wait()s the child so that when kill_orphans
        // SIGKILLs it, the killed child is reaped immediately (clearing
        // /proc/<pid>) rather than lingering as a zombie held by this test.
        // wait_for_death (inside kill_orphans) polls /proc/<pid> and therefore
        // observes the reaping.
        let child = Command::new(&firecracker_bin)
            .arg("30") // plenty long; we SIGKILL it well before it would exit
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn firecracker-named sleep as direct child");
        let pid = child.id();
        // Own the Child entirely in a reaper thread: it blocks on wait() so
        // that when kill_orphans SIGKILLs the process, the killed child is
        // reaped immediately (clearing /proc/<pid>) rather than lingering as
        // a zombie. wait_for_death (inside kill_orphans) polls /proc/<pid> and
        // therefore observes the reaping.
        let reaper = std::thread::spawn(move || {
            let mut child = child;
            let _ = child.wait(); // reap promptly once the test SIGKILLs it
        });

        // Sanity: confirm /proc/<pid>/comm really is "firecracker".
        let _comm = {
            let read = std::fs::read_to_string(format!("/proc/{pid}/comm"))
                .expect("read /proc/<pid>/comm");
            assert_eq!(
                read.trim(),
                "firecracker",
                "test harness requires comm == firecracker, got {read:?}"
            );
            read
        };

        // Record the child's REAL start time so process_identity_matches
        // returns Match (the primary identity check passes).
        let starttime = read_proc_starttime(pid).expect("read child starttime");

        r.insert_sandbox(SandboxInfo {
            id: "sb-firecracker-match".into(),
            snapshot_tag: "py".into(),
            netns: Some("forkd-child-1".into()),
            guest_addr: "10.42.0.2:8888".into(),
            created_at_unix: 1,
            pid: Some(pid),
            proc_starttime: Some(starttime),
            boot_id: read_boot_id(),
            memory_limit_mib: None,
            has_branched: false,
            last_branch_memory_path: None,
            branch_count: 0,
        })
        .unwrap();

        let result = r.kill_orphans().unwrap();
        // The irreversible success path executed: the verified
        // firecracker-named process was killed via pidfd_send_kill and
        // waited for death, and the registry entry was removed.
        assert_eq!(
            result.killed, 1,
            "verified firecracker-named child must be killed, got killed={} \
             (kill_failed={}, pruned_stale={})",
            result.killed, result.kill_failed, result.pruned_stale
        );
        assert_eq!(
            result.kill_failed, 0,
            "kill must succeed for a verified firecracker-named child"
        );
        assert_eq!(
            result.pruned_stale, 0,
            "a killed child is counted under `killed`, not `pruned_stale`"
        );
        assert_eq!(
            r.list_sandboxes().len(),
            0,
            "registry entry must be removed after a successful kill"
        );

        // Defense-in-depth cleanup: ensure the direct child is reaped (it was
        // SIGKILLed by kill_orphans above; the reaper thread eats the zombie or
        // the child if kill_orphans somehow left it alive). The reaper's
        // wait() returns once the process is gone; join it to ensure cleanup.
        let _ = unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        let _ = reaper.join();
    }
}
