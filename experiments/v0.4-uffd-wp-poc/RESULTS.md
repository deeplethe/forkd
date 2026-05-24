# v0.4 Phase 1 PoC — empirical results

Run on the dev box (Ubuntu 24.04, kernel 6.14.0-36-generic, i7-12700)
with `sudo ./target/release/v0_4-uffd-wp-poc`.

## Headline

| Region size | WP arm latency | Snapshot violations |
| ----------: | -------------: | ------------------: |
|    64 MiB   |   199.53 µs    |  0 / 16,384         |
|   256 MiB   |   814.51 µs    |  0 / 65,536         |
| 1024 MiB    |   3.19 ms      |  0 / 262,144        |

**`UFFDIO_WRITEPROTECT` arm latency is linear in region size at ~3 ms / GiB.**
Snapshot consistency invariant (every page is pre-WP-arm content)
holds across all three sizes.

## Implications for the v0.4 design

- A 1 GiB parent VM hits the WP-arming critical section in ~3 ms.
  Adding the vCPU + device state dump (microseconds) keeps the total
  BRANCH pause window well under the design's < 10 ms target.
- For larger parents (4 GiB, 8 GiB) the linear extrapolation gives
  12 ms and 25 ms respectively. These are still a 6×–12×
  improvement over v0.3.4's ~150 ms ext4 floor, but no longer
  sub-10 ms; the < 10 ms claim should be qualified by region size in
  the announcement.
- The handler keeps up with a userspace writer doing ~10M writes/sec
  on the 64 MiB case. Each page faults exactly once (on first write),
  then runs at native mmap speed. This is the "WP fault storm
  doesn't happen for typical workloads" claim in the design doc, now
  with empirical backing.

## Detailed 64 MiB run

```
=== v0.4 Phase 1 PoC: UFFDIO_WRITEPROTECT on memfd ===
Region: 64 MiB (16384 pages of 4096 bytes)

[setup] memfd mmap'd at 0x703fc0200000
[setup] populated 16384 pages with BEFORE patterns in 39.480538ms
[uffd] created (fd=4)
[uffd] registered WP mode, supported ioctls bitmap: 0x17c
[wp] armed UFFDIO_WRITEPROTECT over 64 MiB in 205.366µs  ← v0.4 pause-window analog
[writer] 30806600 writes in 3.000104698s (10268508 writes/sec)
[handler] caught 16384 WP faults
[bulk] copied 0 still-clean pages in 336.534µs

=== Result ===
WP arm latency:           205.366µs
Writer throughput:        30806600 writes in 3.000104698s
WP faults handled:        16384
Pages captured by fault:  16384
Pages captured by bulk:   0
Snapshot pages ok:        16384 / 16384
Snapshot violations:      0

PoC PASSED — snapshot is a consistent point-in-time view.
```

## What this does NOT yet verify

Phase 1 is deliberately scoped to the kernel mechanics outside the
KVM context. The Phase 2+ work in
[`crates/forkd-uffd/`](../../crates/forkd-uffd/) still has to:

- Validate the same invariants when guest accesses go through
  `KVM_RUN` (open question #1 in
  [`DESIGN-v0.4.md`](../../DESIGN-v0.4.md))
- Handle the transparent-hugepage interaction (open question #2)
- Test under realistic kernel-version diversity (≥ 5.7) and graceful
  fallback below
- Stress under write-heavy guests (`stress-ng --vm`)
- Coordinate `UFFDIO_WRITEPROTECT` with KVM dirty-bitmap polling
  (open question #3)

## Reproduce

```bash
# kernel ≥ 5.7
sudo cargo run --release -p v0_4-uffd-wp-poc          # default 64 MiB
sudo REGION_MIB=256  cargo run --release -p v0_4-uffd-wp-poc
sudo REGION_MIB=1024 cargo run --release -p v0_4-uffd-wp-poc
```

Either run as root or `sudo sysctl vm.unprivileged_userfaultfd=1`
first.
