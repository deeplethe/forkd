//! `forkd-controller`: per-host daemon that owns the VM lifecycle.
//!
//! Status: skeleton. Will speak gRPC to clients (Python SDK, CLI),
//! manage per-host scheduling, cgroups, and the parent/child refcount.

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    tracing::info!("forkd-controller starting (stub)");
    anyhow::bail!("not yet implemented: controller serve loop")
}
