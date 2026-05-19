//! In-memory VM registry, snapshotted to a JSON file for crash recovery.
//!
//! Concurrency: a single `parking_lot::Mutex` wraps the whole registry.
//! Writes are infrequent (one per sandbox lifecycle event) so contention
//! is a non-issue at our scale (≤ a few thousand sandboxes/host).
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
    /// Returns Ok(false) if the sandbox is no longer registered (it
    /// got DELETE'd during the BRANCH window — best-effort, the
    /// updated state is silently dropped).
    pub fn mark_branched(&self, id: &str, last_memory_bin: PathBuf) -> Result<bool> {
        let updated = {
            let mut g = self.inner.lock();
            match g.sandboxes.get_mut(id) {
                Some(sb) => {
                    sb.has_branched = true;
                    sb.last_branch_memory_path = Some(last_memory_bin);
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
        let state = self.inner.lock().clone();
        let tmp = self.path.with_extension("json.tmp");
        let body = serde_json::to_vec_pretty(&state)?;
        fs::write(&tmp, &body)
            .with_context(|| format!("write tmp state file {}", tmp.display()))?;
        fs::rename(&tmp, &self.path)
            .with_context(|| format!("rename {} → {}", tmp.display(), self.path.display()))?;
        Ok(())
    }

    /// Prune sandbox entries whose recorded pid is no longer alive.
    /// Snapshots are kept regardless (they're disk artifacts).
    pub fn reconcile(&self) -> Result<usize> {
        let mut pruned = 0usize;
        let stale: Vec<String> = {
            let g = self.inner.lock();
            g.sandboxes
                .iter()
                .filter_map(|(id, sb)| match sb.pid {
                    Some(pid) if !pid_alive(pid) => Some(id.clone()),
                    _ => None,
                })
                .collect()
        };
        for id in stale {
            self.inner.lock().sandboxes.remove(&id);
            pruned += 1;
        }

        // Workspaces (#116): any workspace marked Running whose
        // live_sandbox_id is no longer in the live sandbox table is
        // Stale — the daemon crashed/restarted out from under it.
        // We don't touch Suspended workspaces; they were intentionally
        // parked.
        let live_ids: std::collections::HashSet<String> =
            self.inner.lock().sandboxes.keys().cloned().collect();
        let mut stale_ws_changed = false;
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
                        stale_ws_changed = true;
                    }
                }
            }
        }

        if pruned > 0 || stale_ws_changed {
            self.flush()?;
        }
        Ok(pruned)
    }

    /// For metrics: live counts.
    pub fn counts(&self) -> (usize, usize) {
        let g = self.inner.lock();
        (g.snapshots.len(), g.sandboxes.len())
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::SandboxInfo;
    use tempfile::TempDir;

    #[test]
    fn round_trip_persist_load() {
        let td = TempDir::new().unwrap();
        let path = td.path().join("state.json");

        let r = Registry::load_or_init(&path).unwrap();
        r.insert_sandbox(SandboxInfo {
            id: "sb-1".into(),
            snapshot_tag: "py".into(),
            netns: Some("forkd-child-1".into()),
            guest_addr: "10.42.0.2:8888".into(),
            created_at_unix: 1,
            pid: Some(99999999),
            memory_limit_mib: None,
            has_branched: false,
            last_branch_memory_path: None,
        })
        .unwrap();

        let r2 = Registry::load_or_init(&path).unwrap();
        assert_eq!(r2.list_sandboxes().len(), 1);
        assert_eq!(r2.list_sandboxes()[0].id, "sb-1");
    }
}
