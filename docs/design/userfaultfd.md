# Live branching via userfaultfd

**Status:** **Deferred from v0.3 — see [issue #101](https://github.com/deeplethe/forkd/issues/101).** v0.3 phase 1 (diff snapshots) shipped 143× on the same workload **without** the architecture this doc proposes. The scaffolding (this doc, `crates/forkd-uffd/`, `MemoryBackend::Userfault` enum) stays as honest record + revival starting point. The Firecracker patch we sketched (previously under `firecracker-patch/`) has been removed — see "Why we won't fork Firecracker" below for the reasoning.
**Tracking:** ROADMAP.md → "Live (no-pause) branching via userfaultfd" (deferred).
**Prior art:** MITOSIS (NSDI '23), FaaSnap (ATC '22), Klotski (OSDI '22), NFork (EuroSys '24), CodeSandbox (blog, patched Firecracker).

**Contributor note (2026-07):** do not build new work on the old
`MemoryBackend::Userfault { handler_sock }` restore backend. That arm is
still scaffolding and deliberately bails in `restore_many_with`. The live
BRANCH path that actually shipped is the `MemoryBackend::MemfdShared` +
`forkd_uffd::wp_snapshot::WpBranch` path, where Firecracker registers
`UFFDIO_WRITEPROTECT` on the running source's guest RAM and the controller
copies dirty pages asynchronously.

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

### Answer to issue #257

Keep `MemoryBackend::File` as the default for normal spawn/fan-out.
The file-backed `MAP_PRIVATE` restore path is the memory-efficient
primitive: clean pages are shared by the kernel page cache across all
children, and only divergent pages are copied. A UFFD-backed spawn path
would serve first touches with `UFFDIO_COPY`, which turns shared clean
pages into per-child private copies and loses the main density benefit.

The UFFD-backed work that did ship is for live BRANCH, not ordinary
spawn. That path uses `MemoryBackend::MemfdShared` plus
`WpBranch`: Firecracker maps the running source's RAM from a shared
memfd, registers write-protect userfaultfd on that mapping, and the
controller copies clean and dirtied pages while the source resumes.

The older `MemoryBackend::Userfault { handler_sock }` shape and its
per-child handler socket are historical scaffolding. New work should not
anchor on that socket topology unless the design is reopened; a shared
handler/routing layer remains an open design question, not a committed
production direction.

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

## Why we won't fork Firecracker (decision, 2026-05-19)

Earlier drafts of this doc planned a `deeplethe/firecracker` fork
pinned at v1.10.1 with a `MemoryBackend::Memfd` patch (~100 LOC).
A sketch lived under `firecracker-patch/`. We've deleted that
directory and are explicitly NOT taking the fork path.

The reasoning:

1. **v0.3 phase 1 hit 143× without it.** Diff snapshots — pure
   userspace work on vanilla Firecracker's existing
   `enable_diff_snapshots` + `track_dirty_pages` — got source-pause
   from 29.3 s to 205 ms on 4 GiB SSD. The original v0.3 plan
   targeted ~30 ms; phase 1 already cleared 85 % of that headroom
   without forking. The remaining 175 ms is mostly Firecracker
   control-plane (PUT /snapshot/create round-trip + vCPU state
   harvest), which a memfd path wouldn't shrink.

2. **The memfd value-add isn't sharing.** Today's
   `mmap(memory.bin, MAP_PRIVATE)` across N children already gives
   kernel-CoW page sharing for free. memfd would change the
   abstraction (RAM lives in a kernel memory object instead of a
   file) but doesn't add sharing capability we don't already have.
   Its real value is enabling uffd_wp tracking of source writes
   in a clean way — and that's only useful for the live-fork
   architecture sketched above, which has open questions of its
   own (the source-divergence sync mechanism).

3. **Fork maintenance is real cost.** We'd own our own
   musl-via-docker CI, rebase the patch on every upstream tag
   (Firecracker ships ~quarterly), track upstream CVEs and
   re-issue releases, and weaken the "vanilla Firecracker"
   trust story users rely on. The CodeSandbox blog posts show
   they've taken this path; they also haven't upstreamed in years.

4. **Cheaper alternatives close the remaining gap without a
   fork.** Phase 2 (NVMe + io_uring async writes), phase 3
   (pre-emptive background snapshot with reflink/btrfs CoW where
   available), and phase 1d (per-sandbox shadow file lifting the
   first-BRANCH-only restriction) all address the workloads where
   phase 1's 143× doesn't apply — none of them require touching
   Firecracker.

**What it would take to revive the fork path.** Two of:
- Phase 2/3/1d shipped, measured, and the remaining pause floor
  is still the user-visible bottleneck on a real workload.
- A specific external user (downstream project / paper co-author)
  commits to using `deeplethe/firecracker`.
- An end-to-end sketch of the source-divergence sync mechanism
  that's concrete enough to be a paper section (not a paragraph).
- Firecracker upstream accepts external-memfd injection (we'd
  lose the maintenance cost entirely).

Same revival criteria as issue #101.
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
