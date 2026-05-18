# forkd × CubeSandbox

A thoughtful walk-through of how these two open-source projects relate
and where they can interoperate. Written for engineers from either
team or operators considering deploying both.

## TL;DR

| | forkd | [CubeSandbox](https://github.com/TencentCloud/CubeSandbox) |
|---|---|---|
| **Position** | Fork-on-write microVM **primitive** | Full sandbox **runtime** with cluster scheduling |
| **VMM** | Firecracker | RustVMM (rust-vmm crates, tightly trimmed) |
| **Hypervisor** | KVM | KVM |
| **API surface** | REST `/v1/sandboxes/:id/branch` + Python `Controller` + `forkd` CLI | E2B-compatible SDK + REST `/v2/sandboxes` + dashboard |
| **Cold start** | ~150 ms (read-many from snapshot) | < 60 ms (pool pre-provision + clone) |
| **Per-instance memory** | shared parent CoW; child marginal | < 5 MiB per instance (CoW + trimmed runtime) |
| **Fork-on-write** | ✅ `POST /branch` primitive | 🚧 roadmap: "Event-level snapshot rollback (coming soon)" |
| **Multi-node scheduling** | ❌ single-host today | ✅ cluster with master + nodes |
| **E2B compatibility** | Python SDK only | E2B drop-in replacement |
| **License** | Apache-2.0 | Apache-2.0 |

**The takeaway**: forkd and CubeSandbox are **complementary**. forkd is
sharply focused on the fork primitive; CubeSandbox is a full runtime
with the cluster-side concerns forkd doesn't address. Where their
roadmaps touch — the "rollback / fork-from-snapshot" capability
CubeSandbox lists as "coming soon" — forkd already ships a working
implementation. There's a real opportunity to share code or
co-publish a joint engineering story.

## Where they overlap

Both projects make the same foundational decisions:

1. **KVM, not container.** Hardware isolation is non-negotiable for
   running untrusted AI-generated code.
2. **CoW memory sharing.** Memory amplification (1 parent's RAM
   serving N children) is the only way per-instance overhead stays
   in the single-digit MiB range.
3. **Snapshot/restore as the unit of distribution.** "Ship a warmed
   parent" is much faster than "ship a Dockerfile that builds a
   warmed parent."
4. **AI agent workloads as the target user.** Both projects are
   explicit about this in their READMEs.

## Where they differ

### VMM choice

CubeSandbox went all-in on **rust-vmm crates** assembled into a
custom, aggressively trimmed VMM. This is how they get to
**< 5 MiB per instance** — Firecracker carries machinery they don't
need.

forkd uses **Firecracker** as a dependency. Larger memory footprint
per child (typically 20-50 MiB before guest-side optimization), but:

- Battle-tested in AWS Lambda + Fargate
- Stable API surface; we don't ship VMM source
- Existing recipes (postgres-fixture, langgraph-react, agent-workbench)
  port directly

If you're at the "thousands of concurrent sandboxes on a single node"
end of the spectrum, **CubeSandbox's footprint wins**. If you're at
the "branch a stateful agent and explore 10 paths in parallel" end,
**forkd's primitive is more direct**.

### Fork-on-write

This is the most interesting divergence:

- **forkd**: `POST /v1/sandboxes/:id/branch` is the public, supported
  primitive. Pauses the source VM, writes a snapshot, resumes the
  source, and `mmap`s the snapshot into N children. The
  [pause-window benchmark](../bench/pause-window/RESULTS-v0.2.md)
  measures the trade-off honestly: ~4 s pause on a 513 MiB source.
- **CubeSandbox**: pause/resume endpoints exist (`/sandboxes/:id/pause`,
  `/sandboxes/:id/resume`), but **fork-from-snapshot is listed as
  "coming soon"** in the README. As of writing the README mentions:
  > Event-level snapshot rollback (coming soon): High-frequency
  > snapshot rollback at millisecond granularity, enabling rapid
  > fork-based exploration environments from any saved state.

This is precisely what forkd implements today. If a CubeSandbox user
needs this *now*, they can run forkd alongside CubeSandbox for the
fork-heavy slice of their workload.

### Scheduling

forkd is single-host: one daemon = one machine. The [v0.3 roadmap](../docs/ROADMAP.md)
mentions multi-node scheduling as speculative; we won't get there
until cross-host snapshot diffing lands.

CubeSandbox ships with a CubeMaster / Cubelet / CubeNet architecture
that handles scheduling, networking, and node coordination out of
the box. If you need to scale across machines today, CubeSandbox is
where you go.

## Snapshot format: where the two actually diverge

First, a clarification on terminology: **rust-vmm is not a VMM.**
It's a [collection of Rust crates](https://github.com/rust-vmm)
(`kvm-ioctls`, `vm-memory`, `vm-superio`, `linux-loader`, …) that
provide reusable building blocks for VMMs. **Firecracker uses
rust-vmm crates** — they're the largest consumer and driver of
that ecosystem. CubeSandbox is also built on rust-vmm crates,
assembled into its own custom, aggressively trimmed VMM.

So the real comparison is **Firecracker (full VMM) vs CubeSandbox
(custom VMM assembled from rust-vmm crates)**. Both sit on the
same foundation; the divergence is in everything wrapped around
that foundation. The snapshot format diverges at four layers:

### 1. vCPU register state — *theoretically portable, practically not*

Both VMMs harvest vCPU state through the same KVM ioctls:
`KVM_GET_REGS`, `KVM_GET_SREGS`, `KVM_GET_MSRS`, `KVM_GET_FPU`,
`KVM_GET_XSAVE`, `KVM_GET_LAPIC`, etc. The output structures are
defined by the Linux kernel and are stable across user-space VMMs.

The data is *the same*; the *serialization* isn't. Firecracker
writes a versioned bincode blob with its own schema. CubeSandbox
writes whatever format its team chose. A converter between the
two is mechanically possible (~few hundred lines of Rust) but
needs to track schema changes on both sides.

### 2. Guest memory image — *closest to portable*

Both projects almost certainly use a flat `memory.bin` dump
backed by `mmap(MAP_PRIVATE)` on restore — there's no good
alternative once you've committed to CoW page sharing. Firecracker
documents this explicitly; CubeSandbox's README mentions "extreme
memory reuse via CoW technology" which strongly implies the same
shape.

The bytes themselves are portable in principle: if both sides
target the same guest RAM size and kernel ABI, a Firecracker
`memory.bin` could be `mmap`'d by a CubeSandbox process. But it
won't *boot* on the other side because the vCPU registers (which
reference RAM addresses — `RIP`, page-table pointers, stack
pointer) and the device queue state are tied to the issuing VMM.
Portable memory without portable register state is useless.

### 3. Device state — *the 100% incompatible layer*

This is where the gap is real and fundamental. A complete
snapshot must serialize every virtio device's internal state:

- **virtio-blk**: queue index, descriptor table pointer, pending
  request list, file descriptor identity
- **virtio-net**: tap fd binding, MAC address, queue state,
  in-flight TX/RX buffers
- **serial**: ring-buffer contents, baud rate, control bits
- **MMIO config**: address mappings, IRQ assignments

Firecracker has its own `microvm_state.json` schema for these.
CubeSandbox has its own. **Each VMM has to write its own device
serialization code, and the on-disk formats are not
interchangeable.** This is a hard wall: no converter can paper
over it cleanly because the semantics of "which descriptors are
in-flight" are implementation-specific.

### 4. The intended use case — *the deepest difference*

The four layers above describe *how* snapshots are stored. The
**why** is where forkd and CubeSandbox diverge most:

|                                         | forkd (live branch)                  | CubeSandbox (template clone) |
|---|---|---|
| When the snapshot is captured           | At BRANCH time, from a running VM    | Pre-built once, then immutable |
| Source-VM state at capture              | Live: open TCP, in-flight syscalls   | Quiesced (no in-flight I/O) |
| Device-state requirements               | Must serialize *pending* requests    | Must serialize *clean* state |
| Failure handling                        | Must handle partial-snapshot rollback | Failure = template invalid, rebuild |
| Optimization target                     | Minimize pause window (we hit ~4 s)  | Minimize clone latency (they hit <60 ms) |
| Lifecycle                               | Snapshot lives briefly between BRANCH and spawn | Snapshot lives indefinitely as template |

CubeSandbox's current `pause / resume` endpoints exist, but the
snapshot they would write is a *template* snapshot — captured
from a quiesced VM that doesn't have in-flight virtio descriptors
or open peer sockets. Forking a *live* VM (forkd's BRANCH) is a
harder problem: the device subsystem has to know how to safely
serialize requests that are partway through being processed.

## If CubeSandbox builds fork-on-write themselves

A fair question we'd want a CubeSandbox engineer to ask, openly:
*"Do we need forkd? Can't we just add this ourselves?"*

The honest answer:

**Yes, you can.** The team is competent (5.7k stars on a polished
custom VMM is no accident), the foundational primitives (KVM
pause/resume) are already in place, and the rust-vmm crates
provide the right building blocks.

The cost is not the **lines of code** — it's the **corner cases
that take iteration to discover**.

### Effort scale

| Phase | What it is | Realistic time |
|---|---|---|
| Happy-path implementation | `snapshot/load` REST endpoints + vCPU/memory/device save/restore for the common case | **~3-4 weeks** |
| Corner-case discovery | Hitting + fixing the issues nobody finds until production | **3-6 months** |
| Sub-100 ms restore | Optimization to match your <60 ms cold-start bar for the branch path too | **another 1-2 months** |

The "3 weeks" number is what gets sold internally when proposing
this. The "3-6 months" number is what actually ships.

### Corner cases forkd has already hit (and you'd re-hit)

These aren't theoretical — they're all in the forkd commit log
from the last 7 days:

1. **TCP timestamps / PAWS (RFC 7323).** When a vCPU pauses for
   seconds, its TCP timestamp counter freezes while the peer's
   keeps ticking. The peer's subsequent timestamps look "future"
   to the resumed guest, and PAWS silently drops them.
   Workaround: `echo 0 > /proc/sys/net/ipv4/tcp_timestamps`
   inside every restored VM. We hit this on the langgraph-react
   demo and burned an hour diagnosing it.

2. **kvmclock catch-up semantics.** Whether `CLOCK_MONOTONIC` in
   the guest jumps forward by the pause duration on resume, or
   keeps its pre-pause value, is implementation-specific.
   Firecracker chose "catch up to host TSC". A custom VMM might
   pick differently. Wrong choice → application-level timeouts
   silently misfire, debugging this is hours-to-days.

3. **Stale conntrack on the host bridge.** A new connection from
   a freshly-restored sandbox occasionally hangs for the full
   TLS read-timeout because the host's nf_conntrack table has a
   stale entry from a previous sandbox lifecycle. Pre-warming
   the connection (a throwaway TLS handshake before the agent's
   first real call) papers over it.

4. **tap device naming collisions.** Two sandboxes trying to
   attach to the same `forkd-tap0` fight; you need a per-VM
   allocator. Our `netns_offset` field in `ForkOpts` resolves
   it, but only after we shipped grandchildren-collide-with-
   source as a real bug in PR #52.

5. **Shared rootfs ext4 corruption.** If `/workspace` is in the
   shared rootfs and 3 children write concurrently, the ext4
   journal corrupts within seconds ("Structure needs cleaning").
   We learned to mount `/tmp` as tmpfs in `forkd-init.sh` so
   per-VM mutable state stays in RAM, not on the shared
   rootfs file.

6. **Per-child cgroup v2 limits.** Without `memory.max`, one
   misbehaving child can OOM the host. We wired `memory_limit_mib`
   into `ForkOpts` after the first time a runaway child froze
   the test rig.

7. **In-guest application-level pause semantics.** Our
   [pause-window benchmark](../bench/pause-window/RESULTS-v0.2.md)
   discovered that `socket.recv()` timeouts inside the guest
   *don't fire* during pause (kvmclock-derived `CLOCK_MONOTONIC`
   freezes too, the wait counter doesn't tick) — the recv just
   resumes after pause and returns data before the timer
   notices. Surprising, important, only learnable by measuring.

Each of these is a few hours to a few days of investigation +
fix. The aggregate is the "3-6 months" number.

### Architecture-level point

Beyond the corner cases, there's a deeper concern: **the device
subsystem in a `pause/clone` VMM is allowed to assume
*quiescence* in a way the device subsystem in a
`live-branch` VMM is not.**

CubeSandbox's current snapshot path captures a *quiesced*
template. The virtio-blk queue is empty; the virtio-net buffers
are clean; nothing is half-processed. Forking a *running* VM
with open TCP connections and partway-through file reads needs
the device implementations to safely serialize in-flight state,
which is a different design assumption.

This isn't a "rewrite the VMM" issue — it's a "audit each device
for serializability under in-flight load" issue. Probably the
larger work item inside the 3-6 month estimate.

### What we'd save you

forkd has already paid this engineering cost. If you build the
same primitive in CubeSandbox, you'll re-pay it — there's no
trick that lets you skip the corner cases, just hard work and
iteration. We've documented many of them; the
[pause-window benchmark](../bench/pause-window/RESULTS-v0.2.md)
is a methodology you can borrow even if you don't use the code.

Both projects are Apache 2.0. If you decide to build it yourself
and reference any of forkd's commits / docs / fixes, that's the
license working as intended. We'd love a citation, but we'd love
*any* outcome that pushes fork-on-write microVMs forward in the
open-source ecosystem.

## Integration patterns

Three concrete ways the two can coexist.

### Pattern 1: Side-by-side deployment

The simplest. You run **both** daemons on different ports, route
traffic by use case:

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

Each project owns the workload it's strongest at. The agent talks
to whichever daemon's API matches its current step.

### Pattern 2: forkd as a CubeSandbox `/branch` backend

When CubeSandbox ships the "Event-level snapshot rollback" feature,
it will need *some* implementation strategy. One option: have
CubeSandbox's `/branch` endpoint delegate to a co-located
forkd-controller for the actual snapshot + restore-many work.

This is a real implementation path — both projects are Apache 2.0,
both use KVM, both have stable REST surfaces. The bridge would be:

1. CubeSandbox's `/sandboxes/:id/branch` (proposed) calls into a
   small bridge layer
2. Bridge translates CubeSandbox's internal sandbox identity to a
   forkd sandbox handle (this requires CubeSandbox's pause/resume
   to be compatible with forkd's snapshot format, or a translation
   layer)
3. forkd-controller does the actual pause+snapshot+restore-many
4. CubeSandbox returns the new sandbox handles to the caller

The blocker today: CubeSandbox's RustVMM snapshot format and
forkd's Firecracker snapshot format aren't binary-compatible. A
real implementation would either (a) have both daemons running
their own VMs in parallel, or (b) write a snapshot format
converter — non-trivial but mechanically possible.

**This is where the most interesting collaboration lives.** If
the CubeSandbox team is interested in shipping fork-on-write
without re-implementing it from scratch, forkd has done a lot of
the engineering already.

### Pattern 3: E2B SDK as the lingua franca

Both projects ship E2B-compatible APIs:

- CubeSandbox: drop-in E2B replacement at the daemon level
- forkd: `forkd.Sandbox` Python class matches E2B's surface

If your agent uses the E2B SDK, you can switch backends with one
environment variable. forkd vs CubeSandbox becomes a runtime
configuration choice, not a code change. The fork primitive is
unique to forkd — if your agent doesn't need it, CubeSandbox is a
fine alternative.

## What we'd love to talk about

If you're on the CubeSandbox team and reading this, we'd be
interested in:

- A joint technical blog post on the fork-on-write design space
- A worked example of pattern 1 or 2 above
- Cross-pollination of recipes — your sandbox templates have
  useful properties we'd like to learn from
- An honest comparison benchmark, hosted neutrally

[deeplethe](https://github.com/deeplethe) ships forkd; PR #236 on
your repo (storage cmdTimeout config) is a small example of the
direction we'd be excited to continue.

## See also

- [forkd ROADMAP.md](../docs/ROADMAP.md) — v0.3 userfaultfd plan
- [forkd pause-window benchmark](../bench/pause-window/RESULTS-v0.2.md) — the pause cost we measure today
- [CubeSandbox README](https://github.com/TencentCloud/CubeSandbox)
- [CubeSandbox OpenAPI spec](https://github.com/TencentCloud/CubeSandbox/blob/master/openapi.yml)
- [E2B SDK](https://e2b.dev) — the lingua franca both projects speak
