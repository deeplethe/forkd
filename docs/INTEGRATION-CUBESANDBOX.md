# forkd × CubeSandbox

How these two open-source projects relate technically and where
they can interoperate. Written for engineers from either team
and for operators considering deploying both.

## TL;DR

| | forkd | [CubeSandbox](https://github.com/TencentCloud/CubeSandbox) |
|---|---|---|
| Position | Fork-on-write microVM primitive | Full sandbox runtime with cluster scheduling |
| VMM | Firecracker | Custom VMM built on rust-vmm crates |
| Hypervisor | KVM | KVM |
| API surface | REST `/v1/sandboxes/:id/branch`, Python `Controller`, `forkd` CLI | E2B-compatible SDK, REST `/v2/sandboxes`, dashboard |
| Cold start | ~150 ms (restore-many from snapshot) | <60 ms (pool pre-provision + clone) |
| Per-instance memory | shared parent CoW; child marginal | <5 MiB per instance |
| Fork-on-write | Shipped: `POST /branch` | Roadmap: "Event-level snapshot rollback (coming soon)" |
| Multi-node scheduling | Single-host today | Cluster with master + nodes |
| E2B compatibility | Python SDK only | E2B drop-in replacement |
| License | Apache-2.0 | Apache-2.0 |

forkd and CubeSandbox are complementary. forkd is a focused fork
primitive; CubeSandbox is a full runtime that handles the
cluster-side concerns forkd does not. The capability they have
in common on the roadmap (fork-from-snapshot) is something forkd
ships today and CubeSandbox lists as "coming soon", which is the
natural collaboration point.

## Where they overlap

Both projects make the same foundational decisions:

1. KVM, not container. Hardware isolation is required for running
   untrusted AI-generated code.
2. CoW memory sharing. Memory amplification (one parent's RAM
   serving N children) is what keeps per-instance overhead in
   single-digit MiB.
3. Snapshot/restore as the unit of distribution. Shipping a
   warmed parent is much faster than shipping a Dockerfile that
   builds one.
4. AI agent workloads as the target user. Both projects say so
   explicitly in their READMEs.

## Where they differ

### VMM choice

CubeSandbox assembles its own VMM from rust-vmm crates, trimmed
aggressively. That is how they reach <5 MiB per instance:
Firecracker carries machinery they do not need.

forkd uses Firecracker as a dependency. Larger memory footprint
per child (typically 20-50 MiB before guest-side optimization).
In return:

- Firecracker is production-tested in AWS Lambda and Fargate.
- Stable API surface; we do not ship VMM source.
- Existing recipes (postgres-fixture, langgraph-react,
  agent-workbench) port directly.

For "thousands of concurrent sandboxes on a single node",
CubeSandbox's footprint wins. For "branch a stateful agent and
explore 10 paths in parallel", forkd's primitive is more direct.

### Fork-on-write

The most interesting divergence:

- forkd: `POST /v1/sandboxes/:id/branch` is the public primitive.
  Pauses the source VM, writes a snapshot, resumes the source,
  and `mmap`s the snapshot into N children. v0.2 baseline
  ([RESULTS-v0.2.md](../bench/pause-window/RESULTS-v0.2.md)):
  163 ms tmpfs / 4.26 s SATA SSD for a 513 MiB source. **v0.3
  (shipped 2026-05-19) adds `"diff": true` opt-in** which collapses
  source-pause to ~200 ms regardless of memory size for idle sources
  — 29 s → 205 ms (143×) on 4 GiB SSD;
  ([RESULTS-v0.3.md](../bench/pause-window/RESULTS-v0.3.md) has the
  full curve including the typical-agent-workload 6-15× band).
- CubeSandbox: pause and resume endpoints exist
  (`/sandboxes/:id/pause`, `/sandboxes/:id/resume`), but
  fork-from-snapshot is on the roadmap, not shipped. From the
  README:

  > Event-level snapshot rollback (coming soon): High-frequency
  > snapshot rollback at millisecond granularity, enabling rapid
  > fork-based exploration environments from any saved state.

A CubeSandbox user who needs this capability today can run forkd
alongside CubeSandbox for the fork-heavy slice of the workload.

### Scheduling

forkd is single-host. One daemon serves one machine. The
[v0.3 roadmap](../docs/ROADMAP.md) lists multi-node scheduling as
speculative; it depends on cross-host snapshot diffing landing first.

CubeSandbox ships with a CubeMaster / Cubelet / CubeNet
architecture that handles scheduling, networking, and node
coordination. If you need to scale across machines today,
CubeSandbox is the right tool.

## Snapshot format

First, a terminology note: rust-vmm is not a VMM. It is a
[collection of Rust crates](https://github.com/rust-vmm)
(`kvm-ioctls`, `vm-memory`, `vm-superio`, `linux-loader`, and
others) that provide reusable building blocks. Firecracker uses
rust-vmm crates and is the ecosystem's largest contributor.
CubeSandbox also uses rust-vmm crates, assembled into a custom
VMM.

So the comparison is Firecracker (a complete VMM) versus
CubeSandbox (a custom VMM built from rust-vmm). Both sit on the
same foundation. The snapshot format diverges at four layers.

### vCPU register state

Both VMMs harvest vCPU state through the same KVM ioctls:
`KVM_GET_REGS`, `KVM_GET_SREGS`, `KVM_GET_MSRS`, `KVM_GET_FPU`,
`KVM_GET_XSAVE`, `KVM_GET_LAPIC`. The output structures are
defined by the Linux kernel and are stable across user-space VMMs.

The data is the same; the serialization is not. Firecracker
writes a versioned bincode blob with its own schema. CubeSandbox
writes whatever format its team chose. A converter is mechanically
possible (a few hundred lines of Rust) but has to track schema
changes on both sides.

### Guest memory

Both projects almost certainly use a flat `memory.bin` dump
backed by `mmap(MAP_PRIVATE)` on restore. There is no good
alternative once you have committed to CoW page sharing.
Firecracker documents this explicitly. CubeSandbox's README
description of "extreme memory reuse via CoW technology"
strongly implies the same approach.

The bytes themselves are portable in principle. If both sides
target the same guest RAM size and kernel ABI, a Firecracker
`memory.bin` could be `mmap`'d by a CubeSandbox process. But it
will not boot, because the vCPU registers reference RAM
addresses (RIP, page-table pointers, stack pointer) and the
device queue state is tied to the issuing VMM. Portable memory
without portable register state is not useful.

### Device state

This is the layer with no realistic compatibility path. A
complete snapshot must serialize every virtio device's internal
state:

- virtio-blk: queue index, descriptor table pointer, pending
  request list, file descriptor identity.
- virtio-net: tap fd binding, MAC address, queue state, in-flight
  TX/RX buffers.
- serial: ring-buffer contents, baud rate, control bits.
- MMIO configuration: address mappings, IRQ assignments.

Firecracker uses its own `microvm_state.json` schema for these.
CubeSandbox has its own. Each VMM writes its own device
serialization code, and the on-disk formats are not
interchangeable. A converter cannot paper over this layer
cleanly, because the semantics of "which descriptors are
in-flight" are implementation-specific.

### Use case

The four layers above describe how snapshots are stored. The
deepest difference is what the snapshot is for:

| | forkd (live branch) | CubeSandbox (template clone) |
|---|---|---|
| When captured | At BRANCH time, from a running VM | Pre-built once, then immutable |
| Source VM state at capture | Live: open TCP, in-flight syscalls | Quiesced (no in-flight I/O) |
| Device-state requirement | Must serialize pending requests | Must serialize clean state |
| Failure handling | Partial snapshot rollback | Failure invalidates template, rebuild |
| Optimization target | Minimize source-pause on BRANCH. v0.2: 163 ms tmpfs / 4.26 s SATA SSD (513 MiB). v0.3 `"diff": true`: 205 ms across all memory sizes for idle sources, 6-15× for typical agent workloads. | Minimize clone latency (they hit <60 ms) |
| Lifecycle | Snapshot lives briefly, between BRANCH and spawn | Snapshot lives indefinitely as template |

CubeSandbox's current pause/resume path captures a template
snapshot from a quiesced VM. Forking a live VM is a harder
problem, because the device subsystem has to serialize requests
that are partway through being processed.

## Integration patterns

Three concrete ways the two can coexist.

### Pattern 1: Side-by-side deployment

The simplest pattern. Run both daemons on different ports and
route traffic by use case:

```
                 ┌─────────────────────────┐
                 │  agent orchestrator     │
                 └────┬────────────────┬──┘
                      │                │
       fork-heavy ────┤                ├──── steady-state
       speculative    │                │     scale-out
       exploration    ▼                ▼
                 ┌──────────┐    ┌──────────────┐
                 │  forkd   │    │ CubeSandbox  │
                 │  :8889   │    │  :8088       │
                 └──────────┘    └──────────────┘
                  Firecracker     RustVMM
```

Each project owns the workload it is strongest at. The agent
talks to whichever daemon's API matches its current step.

### Pattern 2: forkd as a CubeSandbox `/branch` backend

When CubeSandbox ships its "Event-level snapshot rollback"
feature, it will need an implementation strategy. One option:
have CubeSandbox's `/branch` endpoint delegate to a co-located
forkd-controller for the snapshot and restore-many work.

Both projects are Apache 2.0, both use KVM, both have stable
REST surfaces. The bridge would look like:

1. CubeSandbox's `/sandboxes/:id/branch` (proposed) calls into a
   small bridge layer.
2. The bridge translates CubeSandbox's internal sandbox identity
   to a forkd sandbox handle. This requires CubeSandbox's
   pause/resume to be compatible with forkd's snapshot format,
   or a translation layer.
3. forkd-controller performs the pause+snapshot+restore-many.
4. CubeSandbox returns the new sandbox handles to the caller.

The blocker today is binary compatibility. CubeSandbox's
snapshot format and Firecracker's are not interchangeable
(see [Device state](#device-state) above). A real implementation
would either run both daemons with their own VMs in parallel, or
write a snapshot format converter. The converter is non-trivial
but mechanically possible.

This is where the most concrete collaboration sits. If
CubeSandbox wants fork-on-write without re-implementing from
scratch, forkd has done much of the engineering already.

### Pattern 3: E2B SDK as the lingua franca

Both projects ship E2B-compatible APIs:

- CubeSandbox: drop-in E2B replacement at the daemon level.
- forkd: `forkd.Sandbox` Python class matches E2B's surface.

If your agent uses the E2B SDK, you can switch backends with one
environment variable. forkd vs CubeSandbox becomes a runtime
configuration choice, not a code change. The fork primitive is
unique to forkd; if your agent doesn't need it, CubeSandbox is a
solid alternative.

## What we'd like to discuss

If you are on the CubeSandbox team, we are interested in:

- A joint technical blog post on the fork-on-write design space.
- A worked example of pattern 1 or 2 above.
- Cross-pollination of recipes. Your sandbox templates have
  properties we would like to learn from.
- An honest comparison benchmark, hosted neutrally.

forkd is shipped by [deeplethe](https://github.com/deeplethe).
PR #236 on your repo (storage cmdTimeout config) is a small
example of the direction we want to continue.

## See also

- [forkd ROADMAP.md](../docs/ROADMAP.md) for the v0.3 userfaultfd plan.
- [forkd pause-window benchmark — v0.2 baseline](../bench/pause-window/RESULTS-v0.2.md) and [v0.3 results](../bench/pause-window/RESULTS-v0.3.md) for current pause numbers.
- [CubeSandbox README](https://github.com/TencentCloud/CubeSandbox).
- [CubeSandbox OpenAPI spec](https://github.com/TencentCloud/CubeSandbox/blob/master/openapi.yml).
- [E2B SDK](https://e2b.dev), the lingua franca both projects speak.
