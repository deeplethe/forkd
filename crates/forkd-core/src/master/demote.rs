//! The idle-demotion loop.
//!
//! Only ever moves a sandbox one tier colder, and only in desired state --
//! the node does the work when it polls. Promotion is never decided here;
//! it happens when a request arrives.

use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;

use super::api::MasterState;
use crate::state::now_secs;

pub async fn demotion_loop(st: Arc<MasterState>) -> Result<()> {
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    loop {
        tick.tick().await;
        if let Err(e) = sweep(&st) {
            tracing::warn!(error = %e, "demotion sweep failed");
        }
    }
}

fn sweep(st: &Arc<MasterState>) -> Result<()> {
    let floor = st.idle.to_frozen.as_secs() as i64;
    let store = st.store.lock();
    for (sb, idle) in store.idle_candidates(floor)? {
        let want = st
            .idle
            .tier_for_idle(Duration::from_secs(idle.max(0) as u64));
        // One step at a time, so a long-idle sandbox walks hot -> frozen ->
        // snapshot -> detached over successive sweeps rather than jumping.
        // Each leg is cheap; a jump would stall the loop on a tar.
        let next = match sb.tier.colder() {
            Some(n) => n,
            None => continue,
        };
        if want >= next && sb.tier != want {
            store.set_tier(&sb.id, next)?;
            tracing::info!(
                sandbox = %sb.id, from = %sb.tier, to = %next, idle_s = idle,
                "demoting"
            );
        }
    }
    let _ = now_secs();
    Ok(())
}
