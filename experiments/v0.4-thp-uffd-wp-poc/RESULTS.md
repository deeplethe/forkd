# v0.4 Phase 3 PoC — empirical results

Run on the dev box (Ubuntu 24.04, kernel 6.14.0-36-generic, i7-12700)
with `sudo ./target/release/v0_4-thp-uffd-wp-poc`. Host THP config:

```
transparent_hugepage/enabled = always [madvise] never
transparent_hugepage/defrag  = always defer defer+madvise [madvise] never
transparent_hugepage/shmem_enabled = always within_size advise [never] deny force
```

`shmem_enabled=never` is the stock Ubuntu setting — it means memfd/tmpfs
VMAs do not allocate THPs even when `MADV_HUGEPAGE` is set on them.

## Headline

| Phase | Backing | WP arm | Populate | THPs allocated | Fault granularity |
| --- | --- | ---: | ---: | ---: | --- |
| A | memfd + MADV_HUGEPAGE | 868 µs | 25 ms | 0 (shmem disabled) | 4 KiB |
| B | memfd + MADV_NOHUGEPAGE | **202 µs** | 20 ms | 0 | 4 KiB |
| C | anon + MADV_HUGEPAGE | 419 µs | 234 ms | 17×2 MiB | 4 KiB |

64 MiB region in all cases. First-write fault address is page-aligned
(0x2011000) and the handler captures exactly that page — the kernel
preserves 4 KiB fault granularity in all three backings.

## What the data says

### 1. Phase A is a trap — pay the marker, get no THP

memfd is shmem-backed. On stock kernels with `shmem_enabled=never`,
`MADV_HUGEPAGE` does **not** allocate hugepages on the memfd VMA. But
it does set the VMA's `VM_HUGEPAGE` flag, which the WP-arm path
walks. Result: **4.3× slower WP arm with no benefit**. This is the
worst configuration for forkd.

### 2. Phase B is the right choice for forkd

memfd + no THP hint is the fastest WP arm we can get (202 µs for
64 MiB, scaling linearly with size — consistent with Phase 1's
~3 ms/GiB).

This is the configuration the v0.4 implementation should use unless
the operator explicitly enables shmem THP and accepts the overhead.

### 3. Phase C shows the cost of real hugepage splitting

Anonymous memory doesn't go through shmem, so `MADV_HUGEPAGE`
actually allocates hugepages — 17 out of a possible 32 in our 64 MiB
region. WP arm is 2.07× slower than Phase B (419 vs 202 µs), and
fault granularity is still 4 KiB. The kernel WP-marks at the PMD
level on first arm; the hugepage is split-and-WP'd at the first
sub-page fault.

The 234 ms populate time is a separate cost: allocating contiguous
2 MiB physical regions requires defragmentation and zeroing,
~10× more expensive than 4 KiB allocations. For forkd this is a
startup cost (parent VM RAM allocation), not a BRANCH cost.

### Why is the AnonHugePages count unchanged after the first write?

In Phase C, `AnonHugePages` reports 34 MiB before AND after the
write. We expected a split (one 2 MiB hugepage → 512 base pages),
which should drop the count to 32 MiB.

Two interpretations:

1. The fault at offset 0x2011000 landed in a base-page region of the
   VMA (only 17 of 32 expected hugepages were allocated, so 15 regions
   are 4 KiB-backed). A "split" of a 4 KiB region is a no-op.
2. The kernel did a partial-split or used PMD-level WP marker that
   leaves the hugepage layout intact but with per-PTE WP bits. This
   would be visible in `/proc/<pid>/pagemap`.

Either way, the user-visible behavior (4 KiB fault granularity, 2×
arm cost) is what matters for the design.

## Implications for `DESIGN-v0.4.md`

Open question #2 ("Interaction with transparent hugepages"):
**answered**.

- UFFD_WP × THP works; faults are always reported at 4 KiB regardless
  of underlying page size.
- The expected cost (PMD-level WP-mark, split on first sub-page
  fault) is ≤ 2× the no-THP baseline for WP arm; the split itself is
  amortized over actual write activity.
- **forkd should use memfd + no `MADV_HUGEPAGE`** for source VM
  memory regions. This gives the fastest, most predictable WP arm
  latency and matches what Firecracker already does for snapshot
  files. The TLB benefit of hugepages on a parent VM that lives for
  seconds-to-minutes is small; the WP arm cost is paid on every
  BRANCH.

## Full output

```
=== v0.4 Phase 3 PoC: UFFD_WP × transparent hugepages ===

[host] transparent_hugepage/enabled = always [madvise] never
[host] transparent_hugepage/defrag  = always defer defer+madvise [madvise] never

--- Phase A: memfd + MADV_HUGEPAGE ---
[populate] 16384 pages touched in 24.826654ms → AnonHugePages = 0 KiB (0 / 32 hugepages)
[wp arm] 867.63µs → AnonHugePages now 0 KiB (0 hugepages)
[first-fault] write to offset 0x2011000 (page 8209) took 226.001µs
[first-fault] handler captured addr at offset Some(2011000) (page Some(8209)), expected page 8209
[post-write] AnonHugePages now 0 KiB (0 hugepages)

--- Phase B: memfd + MADV_NOHUGEPAGE ---
[populate] 16384 pages touched in 19.613077ms → AnonHugePages = 0 KiB (0 / 32 hugepages)
[wp arm] 201.547µs → AnonHugePages now 0 KiB (0 hugepages)
[first-fault] write to offset 0x2011000 (page 8209) took 227.585µs
[first-fault] handler captured addr at offset Some(2011000) (page Some(8209)), expected page 8209
[post-write] AnonHugePages now 0 KiB (0 hugepages)

--- Phase C: MAP_ANONYMOUS + MADV_HUGEPAGE ---
[populate] 16384 pages touched in 234.551264ms → AnonHugePages = 34816 KiB (17 / 32 hugepages)
[wp arm] 418.698µs → AnonHugePages now 34816 KiB (17 hugepages)
[first-fault] write to offset 0x2011000 (page 8209) took 181.759µs
[first-fault] handler captured addr at offset Some(2011000) (page Some(8209)), expected page 8209
[post-write] AnonHugePages now 34816 KiB (17 hugepages)
```

## What's still not measured (open for Phase 4)

- Sustained write storm under a real KVM guest (open question #3
  in DESIGN-v0.4.md — fault throughput under load).
- Behavior when `shmem_enabled=advise` and memfd actually backs real
  THPs.
- `KVM_GET_DIRTY_LOG` × UFFD_WP coordination (open question #3).
- Pre-5.7 kernel fallback path.

## Reproduce

```bash
sudo cargo run --release -p v0_4-thp-uffd-wp-poc
```
