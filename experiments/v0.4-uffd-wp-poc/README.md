# v0.4 Phase 1 PoC — UFFDIO_WRITEPROTECT on memfd

A standalone binary that exercises the kernel mechanics the v0.4
"live-fork" path depends on, *outside* the KVM / Firecracker context.
If this PoC passes, the design proposed in
[`DESIGN-v0.4.md`](../../DESIGN-v0.4.md) is at least kernel-feasible.

## What it does

1. `memfd_create` a 64 MiB region, `mmap(MAP_SHARED)` it.
2. Fill with `PAGE_<idx>_BEFORE` labels.
3. Open a `userfaultfd` with `PAGEFAULT_FLAG_WP`, register the region
   with `UFFDIO_REGISTER_MODE_WP`.
4. **Arm WP** via `UFFDIO_WRITEPROTECT` — time this. This is the v0.4
   "pause window" analog.
5. Start a writer thread: random pages get rewritten with
   `PAGE_<idx>_AFTER`.
6. Start a handler thread: poll the uffd, capture each first-write
   page into a snapshot file at its proper offset, then clear WP for
   that page so the writer can proceed.
7. After the writer stops, bulk-copy still-clean pages into the
   snapshot (they're still WP'd, safe to read directly).
8. Validate: every page in the snapshot **must** start with its
   `BEFORE` label. Any `AFTER` content means the WP ordering invariant
   is broken and the v0.4 correctness argument is wrong.

## Run

Requires Linux kernel ≥ 5.7 (UFFD_WP on shmem). Either run as root,
or set `vm.unprivileged_userfaultfd=1`.

```bash
cargo run --release -p v0_4-uffd-wp-poc
```

Expected output ends with:

```
WP arm latency:          ~1ms
Writer throughput:       ~XM writes in 3s
WP faults handled:       ~tens of thousands
Pages captured by fault: ~thousands
Pages captured by bulk:  ~thousands
Snapshot pages ok:       16384 / 16384
Snapshot violations:     0

PoC PASSED — snapshot is a consistent point-in-time view.
```

## What we're checking

| Question from DESIGN-v0.4.md | How the PoC answers it |
| --- | --- |
| Does `UFFDIO_WRITEPROTECT` work on memfd VMAs? | If the PoC PASSES at all, yes — registration and arming both succeed. |
| What's the arm latency on a real region? | Printed as "WP arm latency" — should be sub-millisecond per GiB on tested kernels. |
| Can the handler keep up under write pressure? | "WP faults handled" vs "Writer throughput" ratio — if the writer outruns the handler, we see snapshot violations. |
| Is the consistency invariant actually maintainable? | "Snapshot violations" count must be 0. |

## What it does NOT cover

- KVM_RUN interaction (a guest accessing memory through KVM, not via
  host userspace writes). That's open question #1 in the design doc.
- Transparent hugepages (open question #2).
- Cross-host or process boundaries.
- Production-shaped error handling, telemetry, or graceful fallback.

Those land in Phase 2+, when this gets integrated into
`crates/forkd-uffd/`.
