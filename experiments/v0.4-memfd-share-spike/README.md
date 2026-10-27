# v0.4 memfd-share spike

A 30-second Python script that answers the question:

> Can a memfd created in process A be opened by process B via
> `/proc/<A_pid>/fd/<N>`, and do writes propagate both ways?

Answer (verified on Ubuntu 24.04, kernel 6.14): **yes**.

This is the empirical foundation for v0.4's no-FC-patch integration
path documented in
[`DESIGN-v0.4-PHASE3-SPIKE.md`](../../DESIGN-v0.4-PHASE3-SPIKE.md).

## Run

```bash
python3 spike.py
```

Expected output ends with `SUCCESS — cross-process memfd open via
/proc/<pid>/fd/N works`.

## Why this matters

Firecracker accepts `mem_backend.backend_path` for snapshot restore.
If sandstorm creates a memfd, populates it with the parent VM's snapshot,
and hands FC the path `/proc/<sandstorm_pid>/fd/<N>`, FC opens that path
and both processes share access to the same memfd. Then sandstorm can
arm `UFFDIO_WRITEPROTECT` on its mmap, FC's guest writes trap to
sandstorm's uffd handler (Phase 2 PoC confirms EPT-mediated writes do
propagate to UFFD_WP on the host VMA), and `WpBranch` captures the
snapshot async — all without modifying Firecracker.

The next step is Phase 5: boot a real FC against `/proc/self/fd/<N>`
and confirm FC's `File` backend uses `MAP_SHARED` semantics.
