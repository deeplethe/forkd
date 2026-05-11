# DESIGN: forkd architecture

Living doc. Will evolve as week 1–4 reveal what reality looks like.

## North-star API

Rust:

```rust
let parent = Vm::boot(image)?.warm_up()?;
let snapshot = parent.snapshot()?;

let children: Vec<Vm> = snapshot.fork_many(ForkOpts { n: 100, ..Default::default() })?;
for child in &children {
    child.exec(["python", "solve.py", &prompt])?;
}
```

Python:

```python
import forkd
parent = forkd.Parent.from_image("python:3.12-slim").preload(["numpy", "torch"])
snap = parent.snapshot()
children = snap.fork(n=100)
for ch in children:
    ch.exec(["python", "-c", code])
```

## Runtime: Firecracker

We build on Firecracker, not gVisor:
- Snapshot / restore is built in.
- KVM-backed; real isolation boundary.
- ~5 MB memory overhead per VM.
- Rust API surface; matches our stack.

**We fork Firecracker initially.** Likely patches we'll carry:
- Per-restore MAC/IP rewrite hook.
- Force `MAP_PRIVATE` on memory restore (already supported via `MEMORY_LOAD_SHARED` close to what we need).
- vsock CID allocation API for child VMs.

Upstream what we can; don't block on it.

## How forking actually works

```
                 Parent VM
                 ─────────
   +-------------+   warmed: python loaded, deps imported
   |  memory.bin |◄──── mmap shared by all children
   +-------------+
   | vmstate.json|     vCPU regs, devices, MMIO state
   +-------------+

         │ snapshot
         ▼

   Child #1, Child #2, ... Child #N
   ──────────────────────────────
   Each in its own Firecracker process:
   - mmap(memory.bin, PROT_READ|PROT_WRITE, MAP_PRIVATE)
       → kernel CoW: reads share, writes allocate new pages
   - Restore vCPU from vmstate.json
   - Patch: new MAC, reseeded RNG, fresh TSC offset
   - Resume vCPU
```

The kernel does the hard part. Our job is correctness + isolation + scheduling.

## The 8 dirty problems

### 1. Memory file backing

Putting `memory.bin` on tmpfs invites OOM kill. Putting it on slow disk kills cold-restore latency.

**Plan**: hugepage-backed file (`memfd_create + MFD_HUGETLB`) or NVMe with `O_DIRECT` + readahead. Benchmark both. Start with hugepages.

### 2. vsock CID collisions

CIDs are namespace-local. N children from one snapshot start with the same CID.

**Plan**: assign each child a fresh CID via the host's vsock CID allocator. If we go virtio-net only, this disappears.

### 3. MAC / IP hot-patch

Guest kernel initialized the NIC during parent warm-up. Changing MAC post-restore confuses the guest.

**Plan**: parent uses `macvtap` with a placeholder MAC. Guest runs a small daemon (started before snapshot) listening on vsock. On restore, host sends "new identity" message; daemon triggers `systemd-networkd` reload with the new MAC/IP. Target ~10 ms.

### 4. RNG / TSC

All children boot with parent's RNG state. Cryptographically broken.

**Plan**: at restore, write fresh bytes from host `/dev/urandom` to guest's `/dev/random` via vsock. Randomize TSC offset (Firecracker config flag).

### 5. Block device CoW

Children need writable rootfs but share base.

**Plan**: overlayfs on host. Lower = parent's rootfs (read-only). Each child gets fresh upper dir. Persist nothing post-exit.

Future: dm-thin for production density; overlayfs is fine for v0.1.

### 6. KSM coordination

Default KSM is too lazy; takes minutes to reach steady-state sharing. We need seconds.

**Plan**: directed KSM hints. Mark fork-family memory regions via `madvise(MADV_MERGEABLE)` immediately on restore. Tune `pages_to_scan`, `sleep_millisecs` per family.

Stretch: kernel patch for "fork-aware KSM" — skip scanning, declare known-shared.

### 7. OOM cascades

If host hits memory pressure and OOM-kills parent, every child loses its backing pages.

**Plan**: reverse refcount in our scheduler. Cgroup `memory.swap.high` to push parent to swap, not kill. Parent is never killed while any child holds it.

### 8. Scheduling affinity

Children must land on the same host as their parent (otherwise CoW becomes copy-everything).

**Plan**: single-host only in v0.1. v0.3 adds "migrate parent to a busier node" semantics.

## Components

```
crates/
  forkd-vmm/         Firecracker wrapper, snapshot/fork logic, MAP_PRIVATE restore.
  forkd-controller/  gRPC server, per-host scheduler, lifecycle, cgroups.
  forkd-cli/         The `forkd` binary.
sdk/
  python/            forkd Python SDK (mirrors Modal's surface where it makes sense).
```

Out of scope for v0.1:
- Multi-host scheduling
- Auth / multi-tenancy
- GPU
- Billing / quotas
- Web UI

## Non-goals

- Replace Modal as a SaaS.
- Beat E2B / Daytona on single-sandbox cold start.
- Support arbitrary OS images. Start with Linux x86-64 + Python guest.

## Open questions

These get answered in the first month:

- Is `MAP_PRIVATE` enough for production CoW, or do we need `userfaultfd`-driven lazy paging from day 1?
- Can we avoid the in-guest vsock daemon? (Maybe rewrite identity via PCI config space.)
- Use a shim above Firecracker (firepilot, vmpath) or vendor it directly?
- Overlayfs vs dm-thin: at what N does overlayfs fall over?
