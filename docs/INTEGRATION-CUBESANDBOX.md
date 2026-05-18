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
  and `mmap`s the snapshot into N children. The
  [pause-window benchmark](../bench/pause-window/RESULTS-v0.2.md)
  measures the cost: about 4 seconds of pause on a 513 MiB source.
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
| Optimization target | Minimize pause window (we hit ~4 s) | Minimize clone latency (they hit <60 ms) |
| Lifecycle | Snapshot lives briefly, between BRANCH and spawn | Snapshot lives indefinitely as template |

CubeSandbox's current pause/resume path captures a template
snapshot from a quiesced VM. Forking a live VM is a harder
problem, because the device subsystem has to serialize requests
that are partway through being processed.

## If CubeSandbox builds fork-on-write themselves

A fair question for a CubeSandbox engineer to ask: do we need
forkd, or can we add this ourselves?

The honest answer is yes, you can. The team is competent (5.7k
stars on a polished custom VMM is not an accident), the
foundational primitives (KVM pause/resume) are already in place,
and rust-vmm provides the right building blocks.

The cost is not in lines of code. It is in the corner cases that
take iteration to discover.

### Effort scale

| Phase | What it is | Realistic time |
|---|---|---|
| Happy-path implementation | `snapshot/load` REST endpoints, vCPU/memory/device save/restore for the common case | ~3-4 weeks |
| Corner-case discovery | Hitting and fixing the issues nobody finds until production | 3-6 months |
| Sub-100 ms restore | Optimization to match your <60 ms cold-start bar on the branch path | another 1-2 months |

The 3-week number is what gets proposed internally. The 3-6 month
number is what actually ships.

### Corner cases we've hit

These are all in the forkd commit log from the last seven days.
Each one is a few hours to a few days of investigation plus fix;
in aggregate they account for the 3-6 month estimate.

1. **TCP timestamps and PAWS (RFC 7323).** When a vCPU pauses
   for seconds, the guest's TCP timestamp counter freezes while
   the peer's keeps ticking. The peer's later timestamps look
   future-dated to the resumed guest, and PAWS silently drops
   them. Workaround: write `0` to
   `/proc/sys/net/ipv4/tcp_timestamps` inside every restored VM.
   We hit this on the langgraph-react demo and spent an hour
   diagnosing it.

2. **kvmclock catch-up semantics.** Whether `CLOCK_MONOTONIC` in
   the guest jumps forward by the pause duration on resume, or
   keeps its pre-pause value, is implementation-specific.
   Firecracker chose "catch up to host TSC". A custom VMM might
   choose differently. The wrong choice produces silently
   misfiring application-level timeouts.

3. **Stale conntrack on the host bridge.** The first connection
   from a freshly-restored sandbox occasionally hangs for the
   full TLS read-timeout because the host's nf_conntrack table
   holds a stale entry from a previous sandbox lifecycle.
   Pre-warming with a throwaway TLS handshake before the agent's
   first real call masks the issue.

4. **Tap device naming collisions.** Two sandboxes attaching to
   the same `forkd-tap0` will fight. You need a per-VM allocator.
   Our `netns_offset` field in `ForkOpts` resolves this, after
   we shipped grandchildren-collide-with-source as a real bug in
   PR #52.

5. **Shared rootfs ext4 corruption.** If `/workspace` lives in
   the shared rootfs and three children write concurrently, the
   ext4 journal corrupts within seconds ("Structure needs
   cleaning"). Mounting `/tmp` as tmpfs in `forkd-init.sh` keeps
   per-VM mutable state in RAM, off the shared rootfs.

6. **Per-child cgroup v2 limits.** Without `memory.max`, a
   misbehaving child can OOM the host. We wired `memory_limit_mib`
   into `ForkOpts` after the first time a runaway child froze
   the test rig.

7. **In-guest application-level pause semantics.** Our
   [pause-window benchmark](../bench/pause-window/RESULTS-v0.2.md)
   found that `socket.recv()` timeouts inside the guest do not
   fire during pause. The kvmclock-derived `CLOCK_MONOTONIC`
   freezes too, the wait counter does not tick, and on resume
   the recv returns data before the timer notices. Surprising
   but important; only visible if you measure.

### Device subsystem assumptions

Beyond the corner cases, there is a deeper concern: the device
subsystem in a pause/clone VMM is allowed to assume quiescence
in a way a live-branch VMM is not.

CubeSandbox's current snapshot path captures a quiesced template.
The virtio-blk queue is empty; the virtio-net buffers are clean;
nothing is half-processed. Forking a running VM with open TCP
connections and partway-through file reads requires each device
implementation to safely serialize in-flight state. That is a
different design assumption.

This is not a "rewrite the VMM" issue. It is an audit-each-device
issue: confirming each implementation tolerates serialization
under in-flight load, and fixing the ones that don't. Probably
the larger work item inside the 3-6 month estimate.

### What forkd would save you

If CubeSandbox builds the same primitive, you will pay the same
engineering cost. There is no shortcut around the corner cases.
We have documented many of them; the
[pause-window benchmark](../bench/pause-window/RESULTS-v0.2.md)
is a methodology you can reuse even without our code.

Both projects are Apache 2.0. If you build it yourself and
reference forkd's commits, docs, or fixes, that is the license
working as intended. A citation is appreciated, but the outcome
that matters most is fork-on-write microVMs moving forward in
the open-source ecosystem.

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
- [forkd pause-window benchmark](../bench/pause-window/RESULTS-v0.2.md) for the pause cost we measure today.
- [CubeSandbox README](https://github.com/TencentCloud/CubeSandbox).
- [CubeSandbox OpenAPI spec](https://github.com/TencentCloud/CubeSandbox/blob/master/openapi.yml).
- [E2B SDK](https://e2b.dev), the lingua franca both projects speak.
