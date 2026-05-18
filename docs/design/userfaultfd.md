# Live branching via userfaultfd

**Status:** v0.3 design draft. Scaffolding only — no production code.
**Tracking:** ROADMAP.md → "Live (no-pause) branching via userfaultfd".
**Prior art:** MITOSIS (NSDI '23), FaaSnap (ATC '22), Klotski (OSDI '22), NFork (EuroSys '24).

This document is the architectural design for forkd v0.3. It corrects
a framing error in the previous ROADMAP entry (which conflated
pause-window with child cold-start) and proposes a write-protect-based
live-fork design that targets pause-window directly.

## What's actually slow today

Two distinct latencies, often confused:

1. **Pause-window.** Source VM is paused while `vm.snapshot_to()`
   writes a fresh `memory.bin` to disk. Measured in
   [`bench/pause-window/RESULTS-v0.2.md`](../../bench/pause-window/RESULTS-v0.2.md):
   163 ms (tmpfs) to 4.26 s (SATA SSD) for a 513 MiB source, scaling
   linearly with memory. **The 4-second pain.**
2. **Child cold-start.** Each child VM's restore overhead from
   PUT /snapshot/load complete to first vCPU instruction. ~150 ms in
   v0.2, dominated by mmap setup + vCPU restore. Not the user-visible
   problem.

The earlier ROADMAP entry described userfaultfd as a fix for "cold-start
floor from ~150 ms to ~10–30 ms," which targets latency (2). That's
true but it's not the latency anyone complains about. The user-visible
problem is (1), the pause-window. **This design targets (1).**

## Why a naive UFFD backend doesn't work

The obvious idea — replace `mem_backend.backend_type = "File"` with
`"Uffd"` — has two problems:

1. **It doesn't reduce pause-window.** With UFFD, Firecracker still
   needs source's memory written out (or otherwise made addressable)
   before children can be restored. The work moves around but the
   total bytes-to-move doesn't change.

2. **It breaks fan-out CoW sharing.** Today, `mmap(memory.bin,
   MAP_PRIVATE)` across N children gives kernel-managed page cache
   deduplication: clean pages are shared, dirty pages are CoW'd
   per-child. With UFFD, each fault is served by **UFFDIO_COPY**
   (which copies) — so N children end up with N private copies of
   every page they touch. Kernel-CoW is gone. This sacrifices forkd's
   killer move (memory-efficient fan-out) for negligible pause-window
   win.

Source: Firecracker docs on snapshot-resume page-fault handling
(v1.10.1), reviewed against `src/firecracker/examples/uffd/` sample
handlers and the CodeSandbox blog post on UFFD page sharing.

## The design we actually want: UFFD_WP-mediated live fork

The MITOSIS / NFork approach. Sketch:

```
                   ┌──────────────────┐
                   │ source VM (live) │ ← keeps running
                   └────────┬─────────┘
                            │
                            │ guest RAM mapped to a memfd_create
                            │ region (not a tmpfs file)
                            ▼
                   ┌──────────────────────────────────┐
                   │ shared backing memfd (anon)      │
                   │ — registered with uffd_wp        │
                   └──────────────────────────────────┘
                            ▲
                            │ MAP_PRIVATE of the SAME memfd
                            │
              ┌─────────────┼─────────────┐
              │             │             │
        ┌─────┴────┐  ┌─────┴────┐  ┌─────┴────┐
        │ child 1  │  │ child 2  │  │ child 3  │
        └──────────┘  └──────────┘  └──────────┘
```

**At BRANCH time:**

1. Source VM is briefly paused (target: <30 ms).
2. Source's guest RAM is **already** backed by a memfd (set up at
   source-creation time, not BRANCH time). The memfd is registered
   with uffd in WP mode.
3. Children spawn and MAP_PRIVATE the same memfd. They get a
   point-in-time view of source's memory **as of pause time**.
4. Source resumes. Any write by source triggers a uffd_wp event:
   - Handler copies the original page into a "pre-fork backup" area.
   - Children whose mmap covers that page get updated to point at
     the backup (preserving their pre-fork view).
   - Source's write is allowed to proceed.
5. Pause-window cost ≈ uffd registration + vCPU state harvest.
   Independent of guest RAM size. **Target: 30 ms regardless of
   whether source is 512 MiB or 32 GiB.**

The key insight: instead of "freeze source, write its memory to
disk, then let children read from disk," we "freeze source briefly,
mark its memory write-protected, let children share the memory
directly, and resolve concurrent writes lazily."

## What needs to be built

| Component | Where | Effort |
|---|---|---:|
| `MemoryBackend::Userfault` enum variant in `forkd-vmm` | crates/forkd-vmm/src/lib.rs | **Done (phase 0).** |
| `forkd-uffd-handler` binary — UDS handshake | crates/forkd-uffd/ | **Done (phase 1).** |
| `MemoryBackend::Memfd` Firecracker patch (fork the v1.10.1 tag) | deeplethe/firecracker (new repo) | 1 week |
| `scripts/setup-host.sh` switch to forked Firecracker build | forkd repo | 1 day |
| Wire `restore_many_with` to spawn handler, create memfd, pass over UDS | forkd-vmm | 3 days |
| uffd_wp event loop: page-copy + per-child mapping updates | forkd-uffd | 1 week |
| Pause-window benchmark for memfd path | bench/pause-window | 2 days |
| Diff-snapshot harvest for durability (so memfd isn't the only copy) | forkd-vmm | 1 week |
| Recipe + paper-grade A/B (postgres-fixture, vllm) | bench, paper-draft repo | 2 weeks |

Realistic total: **4-6 weeks of focused work**, matches the ROADMAP estimate.

## Open questions

1. **memfd-backed guest RAM** *(answered — Firecracker patch is required).*
   Firecracker v1.10.1 has zero public knobs that accept an externally
   created memfd or fd-as-path for guest memory. The swagger schema
   (`src/firecracker/swagger/firecracker.yaml`) defines exactly two
   `MemoryBackend` strategies — `File` and `Uffd` — and there is no
   CLI flag, env var, or API field that injects an fd.

   The good news: Firecracker's memory module **already has** the
   memfd machinery in-tree — `GuestMemoryMmap::memfd_backed`
   (`src/vmm/src/vstate/memory.rs:127-147`) and the underlying
   `create_memfd` helper exist and are used today, but only when
   *any* attached block device is vhost-user. Adding a third
   `MemoryBackend::Memfd` variant that takes a memfd over the existing
   UDS handshake (alongside the uffd fd, both as `SCM_RIGHTS`
   ancillary data) is ~100 LOC plumbing on top of code that's
   already correct.

   **Direct precedent**: CodeSandbox patched Firecracker for exactly
   this (`memfd_create` in the uffd handler, fd sent to Firecracker
   over the UDS, kernel mmap of the fd). They have not upstreamed.
   Their two blog posts on the technique describe the wire format
   roughly enough to clone the approach.

   **Decision**: maintain a `deeplethe/firecracker` fork pinned at
   the v1.10.1 tag with the `MemoryBackend::Memfd` patch applied.
   `scripts/setup-host.sh` switches its download URL to the fork's
   release. forkd documents the patch as required for v0.3; v0.2
   continues to work with vanilla Firecracker.
2. **uffd_wp + Firecracker compatibility**. Firecracker's UFFD support
   is for snapshot restore, not for write-protecting a live VM. We may
   need to register uffd directly against guest pages from outside
   Firecracker's process, which requires sharing the guest memory fd.
   This is the same memfd dependency as (1).
3. **vCPU state harvest cost**. The 30 ms target assumes vCPU state
   capture is cheap. Firecracker's snapshot_to does
   vmstate (vCPU + device state) + memory.bin (RAM). The vmstate part
   is small (<10 MiB typically) and is what we keep. Measure: how long
   does Firecracker take to write just the vmstate, no memory.bin?
4. **Durability story**. With memfd-backed RAM, "snapshot" no longer
   means "file on disk." We need a separate path that periodically
   sponges memfd pages to disk (or uses diff snapshots) so a host
   crash doesn't lose all state. This is the "where do durable
   snapshots live" question; v0.3 may explicitly defer durability and
   document the trade-off.
5. **Hub integration**. The Hub today ships
   `.forkd-snapshot.tar.zst` packs containing memory.bin. A
   memfd-backed source can be PRE-populated from such a pack on
   creation; live-fork doesn't change the Hub format. But
   **forkd snapshot --from-sandbox** (re-snapshotting a live
   memfd-backed VM) needs to do diff-snapshot-style page harvest
   instead of a full memory.bin write.

## Phasing

This is the working sequencing. Each phase is a separate PR.

| Phase | Scope | PR shape | Acceptance |
|---|---|---|---|
| **0** (this doc) | Design + scaffolding | `MemoryBackend::Userfault` enum variant, doc, no behavior change | Compiles. CHANGELOG entry. |
| **1** | Firecracker uffd handshake | `forkd-uffd` crate with a no-op handler that accepts the UDS connection, receives `(uffd_fd, regions)`, exits. | Unit test: spawn handler, simulate firecracker connect. |
| **2** | memfd-backed source RAM via patched Firecracker (~100 LOC patch on the v1.10.1 tag; published to deeplethe/firecracker fork). | Spawn source with memfd, verify guest sees memory. | `forkd snapshot --from-sandbox` works against a memfd-backed source. |
| **3** | uffd_wp event loop | Real handler that serves UFFDIO_COPY and tracks per-child mapping shifts. | Two children fork off a memfd source, modify their RAM independently, verify isolation. |
| **4** | Pause-window measurement | New `bench/pause-window/v0.3/` directory comparing v0.2 File backend vs v0.3 UFFD path on 256 MiB / 2 GiB / 8 GiB sources. | Pause-window < 50 ms across all sizes. Publish RESULTS-v0.3.md. |
| **5** | Paper draft | HotInfra '26 submission target. | Submitted. |

## Out of scope

- Cross-host live branching (RDMA / NICs with kernel-bypass).
- Persistent fault-handler dump-and-replay (handler crash means VMs hang).
- Fault-driven prefetch policies (predictive page fetch based on
  observed agent traces).

These belong in v0.4+ if the v0.3 numbers justify a follow-up.

## What landed in scaffolding (phase 0)

- `MemoryBackend::File` (default, current behavior) and
  `MemoryBackend::Userfault { handler_sock: PathBuf }` enum variants
  in `crates/forkd-vmm/src/lib.rs`.
- `Snapshot::restore_many_with` accepts a `MemoryBackend` field on
  `ForkOpts`. The `Userfault` arm `bail!`s with a pointer to this doc.
- This document.

No production code path enables the Userfault variant yet. The CLI
flag and daemon REST field are deliberately omitted from phase 0 —
adding them with a `bail!()` backend would mislead users into
thinking the feature exists.

## What landed in phase 1

- New workspace member `crates/forkd-uffd/`:
  - `lib.rs`: `GuestRegionUffdMapping` (wire-compatible with
    Firecracker v1.10.1's `uffd_utils.rs`) and a `handshake` module
    implementing `recvmsg` + `SCM_RIGHTS` to receive the uffd fd plus
    the region descriptor JSON in one syscall.
  - `main.rs`: `forkd-uffd-handler` binary. `--socket <path>` accepts
    one Firecracker connection, logs the regions, and exits. `--log-only`
    leaves the uffd fd open (so the guest will hang on first fault) —
    a debug helper, not production.
- Round-trip handshake test paired over `socketpair(2)` exercises the
  parser without needing a real Firecracker.

What phase 1 does **not** yet do:
- No `UFFDIO_REGISTER` / `UFFDIO_COPY` / `UFFDIO_WAKE` — those need
  the `userfaultfd` crate and land in phase 3.
- No memfd-backed source RAM — that's phase 2 and requires either a
  Firecracker patch or a wrapper that pre-creates the memfd before
  spawning Firecracker.
- No integration with `forkd-vmm`'s `restore_many_with` — the
  Userfault arm still `bail!`s. Wiring happens after phase 2.
