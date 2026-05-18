//! `forkd-uffd-handler` — the userfaultfd page-fault handler binary.
//!
//! Phase 1 scope: accept Firecracker's UDS handshake, log the received
//! memory regions, and exit. The real `UFFDIO_COPY` event loop is
//! phase 3 — see `docs/design/userfaultfd.md`.
//!
//! Typical invocation (driven by `forkd-controller`'s daemon, not by a
//! human directly):
//!
//! ```bash
//! forkd-uffd-handler \
//!     --socket /var/run/forkd/uffd-child-1.sock \
//!     --backing /var/lib/forkd/snapshots/foo/memory.bin
//! ```
//!
//! The handler listens on `--socket`, waits for Firecracker to
//! `connect()`, completes the handshake, and (in phase 1) exits. The
//! `--backing` argument is forward-looking: phase 3's COPY loop will
//! mmap it once and serve faults from it.

#[cfg(not(target_os = "linux"))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("forkd-uffd-handler is Linux-only (depends on userfaultfd)")
}

#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> {
    use anyhow::Context;
    use clap::Parser;
    use std::path::PathBuf;

    #[derive(Parser, Debug)]
    #[command(version, about = "forkd userfaultfd page-fault handler (v0.3 phase 1)")]
    struct Cli {
        /// Unix-domain socket to listen on. Must not already exist.
        /// Firecracker connects here at PUT /snapshot/load time when
        /// `mem_backend.backend_type == "Uffd"`.
        #[arg(long)]
        socket: PathBuf,
        /// Path to the backing memory.bin. Phase 1 doesn't actually
        /// open this — it's recorded for the audit log and will be
        /// consumed by phase 3's UFFDIO_COPY loop.
        #[arg(long)]
        backing: Option<PathBuf>,
        /// If set, after receiving the handshake the handler enters
        /// a debug event loop that reads uffd events and logs them
        /// without serving them. Useful for verifying the round-trip
        /// in tests; leaves the guest VM hung on first fault.
        #[arg(long)]
        log_only: bool,
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    tracing::info!(
        socket = %cli.socket.display(),
        backing = ?cli.backing,
        log_only = cli.log_only,
        "forkd-uffd-handler starting (v0.3 phase 1 — no-op event loop)",
    );

    let hs = forkd_uffd::handshake::accept_handshake(&cli.socket)
        .context("complete firecracker handshake")?;
    tracing::info!(
        regions = hs.regions.len(),
        total_bytes = hs.regions.iter().map(|r| r.size as u64).sum::<u64>(),
        "handshake complete",
    );
    for (i, r) in hs.regions.iter().enumerate() {
        tracing::info!(
            idx = i,
            base = format_args!("{:#x}", r.base_host_virt_addr),
            size = r.size,
            offset = r.offset,
            page_size_kib = r.page_size_kib,
            "region",
        );
    }

    if cli.log_only {
        tracing::warn!(
            "log-only mode: reading uffd events without serving them. \
             The guest VM will hang on the first page fault. This is a \
             debug mode — do NOT use against a production VM."
        );
        // The userfaultfd crate's event loop would go here. Phase 1
        // deliberately stops at "we got the fd" so we can validate the
        // handshake without committing to an event loop shape that
        // phase 3 will rewrite. The fd drops with `hs` on return,
        // closing the kernel-side uffd.
    } else {
        tracing::info!("phase 1 done — handshake validated, exiting");
    }

    Ok(())
}
