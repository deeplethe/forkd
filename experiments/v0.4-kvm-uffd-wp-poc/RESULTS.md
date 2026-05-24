# v0.4 Phase 2 PoC — empirical results

Run on the dev box (Ubuntu 24.04, kernel 6.14.0-36-generic, i7-12700)
with `sudo ./target/release/v0_4-kvm-uffd-wp-poc`.

## Headline

| Metric | Value |
| ------ | ----- |
| WP arm latency (1 MiB) | 9.3 µs |
| Guest runtime (mov + hlt + 1 fault) | 211 µs |
| uffd faults caught | 1 (at GPA 0x1000) |
| Fault flags | 0x3 = `UFFD_PAGEFAULT_FLAG_WRITE \| UFFD_PAGEFAULT_FLAG_WP` |
| Live memfd byte at 0x1000 | 0x42 (AFTER — guest write landed) |
| Snapshot byte at 0x1000 | 0xBE (BEFORE — captured pre-write) |

**Answer to `DESIGN-v0.4.md` open question #1: yes.** UFFD_WP armed on a
host VMA does catch KVM guest writes through EPT, and the handler can
copy the page's pre-write content before the guest write commits.

## Full output

```
=== v0.4 Phase 2 PoC: UFFD_WP × KVM guest writes ===

[setup] memfd mmap'd at 0x7cd46e2a0000, size 1024 KiB
[setup] wrote BEFORE marker 0xbe to GPA 0x1000
[setup] placed 6-byte guest code at GPA 0x100
[kvm] vcpu set to CS:IP = 0:0x100
[uffd] registered WP mode, ioctls bitmap: 0x17c
[uffd] armed UFFDIO_WRITEPROTECT in 9.283µs

[kvm] running vcpu...
[handler] caught fault at GPA 0x1000 (flags=0x3, write=true)
[kvm] guest halted normally in 211.003µs (1 exits)

=== Result ===
WP arm latency:        9.283µs
uffd faults caught:    1 ([(4096, 3)])
Live memfd[0x1000]:  0x42 (expected 0x42 = AFTER)
Snapshot[0x1000]:    0xbe (expected 0xbe = BEFORE)

PoC PASSED — open question #1 answered: yes, UFFD_WP catches KVM guest
writes through EPT, and the pre-write content is captured before the
guest write commits.
```

## Mechanism (best current understanding)

When we arm `UFFDIO_WRITEPROTECT` on the host VMA backing a KVM
memslot, the chain of events on the next guest write is:

1. Guest executes `mov [0x1000], al`.
2. CPU does an EPT lookup. Either the EPT entry is absent (cold,
   triggers EPT violation) or it was previously populated; in our
   PoC it's the first write so EPT-violation fires.
3. KVM's EPT-violation handler in the kernel resolves the GPA to an
   HVA via the memslot, then calls `gfn_to_pfn` to get the host page.
4. `gfn_to_pfn` walks the host page table. UFFD_WP is implemented at
   the host PTE level (via `UFFD_WP_PTE`); the PTE shows write
   blocked.
5. `gfn_to_pfn` invokes the uffd path with `UFFD_PAGEFAULT_FLAG_WP`
   set, queuing an event on the uffd fd.
6. The faulting vcpu thread (which is also the uffd handler's poll
   target in a real impl, but a separate thread in this PoC) blocks
   until the handler reads + responds.
7. Our handler reads the page (still WP'd), writes it to the
   snapshot, clears WP, signals the faulting thread.
8. KVM retries `gfn_to_pfn`, gets a writable host PTE this time,
   installs the EPT entry with W permission, retries the guest
   write. Write commits to memfd.

Step 7 is where the consistency guarantee comes from: the handler
captures the page **before** the EPT entry has W permission, so
the guest cannot have written yet.

## What this means for v0.4

- The fundamental kernel mechanism works. v0.4's BRANCH design is no
  longer waiting on a "does kvm even let us do this" risk; it's now
  an engineering task (Phase 2 integration).
- The fault path adds latency for the *first* write to each page in
  the parent VM after WP is armed: roughly the handler's copy time
  plus a context switch. We measured 200µs for the entire vcpu
  run + 1 fault in the PoC; in practice each fault is maybe 10-30 µs.
- For a write-heavy parent VM, the cumulative fault cost can exceed
  the v0.3.4 pause window. This is the "fault storm" risk in
  DESIGN-v0.4.md; quantifying it (Phase 3) is the next experiment.

## What's still untested

- Transparent hugepages (open question #2 in DESIGN-v0.4.md).
- Sustained write workload (does the handler keep up over millions
  of faults?).
- Multi-vcpu guests racing on the same page.
- The `KVM_GET_DIRTY_LOG` × UFFD_WP interaction (open question #3).
- Older kernels (≤ 5.6) — graceful fallback when UFFD_WP isn't
  advertised.

## Reproduce

```bash
# kernel ≥ 5.7, run as root (or sysctl vm.unprivileged_userfaultfd=1)
sudo cargo run --release -p v0_4-kvm-uffd-wp-poc
```
