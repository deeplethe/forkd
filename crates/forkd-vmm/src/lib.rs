//! `forkd-vmm`: Firecracker wrapper with snapshot/fork primitives.
//!
//! See `DESIGN.md` at the repo root for the architecture.
//!
//! Status: skeleton. Real implementation lands in week 1–4.

use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum VmmError {
    #[error("not yet implemented: {0}")]
    Todo(&'static str),
}

pub type Result<T> = std::result::Result<T, VmmError>;

/// A running Firecracker microVM.
#[derive(Debug)]
pub struct Vm {
    // TODO: API socket path, process handle, current state.
}

/// On-disk snapshot of a paused VM.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub vmstate: PathBuf,
    pub memory: PathBuf,
}

/// Options controlling a fork-many operation.
#[derive(Debug, Clone)]
pub struct ForkOpts {
    pub n: usize,
    // TODO: per-child overlay dir template, netns prefix, CID allocator handle, etc.
}

impl Default for ForkOpts {
    fn default() -> Self {
        Self { n: 1 }
    }
}

impl Vm {
    /// Boot a fresh VM from a rootfs + kernel.
    pub fn boot(_image: PathBuf) -> Result<Self> {
        Err(VmmError::Todo("Vm::boot"))
    }

    /// Run user-supplied warm-up commands; pause when done.
    pub fn warm_up(self) -> Result<Self> {
        Err(VmmError::Todo("Vm::warm_up"))
    }

    /// Snapshot a paused VM: emits `vmstate` + `memory.bin`.
    pub fn snapshot(&self) -> Result<Snapshot> {
        Err(VmmError::Todo("Vm::snapshot"))
    }
}

impl Snapshot {
    /// Spawn `opts.n` children, each restored from this snapshot.
    ///
    /// Each child gets its own Firecracker process, its own netns, its own
    /// overlay rootfs upper dir, and a re-seeded RNG / fresh TSC offset.
    /// Memory is `mmap(MAP_PRIVATE)`'d so reads share parent pages, writes CoW.
    pub fn fork_many(&self, _opts: ForkOpts) -> Result<Vec<Vm>> {
        Err(VmmError::Todo("Snapshot::fork_many"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boot_returns_todo() {
        let err = Vm::boot("/tmp/nope".into()).unwrap_err();
        assert!(matches!(err, VmmError::Todo(_)));
    }
}
