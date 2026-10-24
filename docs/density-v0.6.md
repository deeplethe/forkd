# v0.6 density on one node

> **Everything below was first taken with `--network=host`, before each
> sandbox got its own network namespace.** Both the fill curve and the
> tier latencies have since been re-measured under namespaces; the two
> sections at the end carry the current numbers and the old ones are kept
> only for comparison. The 1000-sandbox idle figure remains an
> extrapolation.


Measured 2026-10-24 on `bm`: 2 vCPU, 3654 MiB RAM, 2047 MiB swap, 62 GB free
disk, ext4, gVisor runsc release-20261019.0. Driven entirely through the
forkd API; nothing in the harness touches runsc.

The sandbox under test is an idle agent: a Python HTTP server plus 3 MiB of
incompressible working set. Zeroes would compress and flatter the numbers.

"Sold" is nominal — what a customer would be told they get. These sandboxes
genuinely use what the table shows.

## Filling the node

Created 10 at a time until the master declined.

| sandboxes | MemAvailable | per sandbox | swap used | PSI some avg10 |
| --------- | ------------ | ----------- | --------- | -------------- |
| 10        | 2612 MiB     | 23.3 MiB    | 503 MiB   | 0.2            |
| 50        | 1649         | 23.9        | 503       | 0.1            |
| 100       | 426          | 24.2        | 505       | 0.1            |
| →         | —            | —           | —         | **declined**   |

Admission stopped placement at 100: `464 MiB free, below the 512 MiB floor`.
Swap never engaged and pressure never rose. Per-sandbox cost was flat at
23–24 MiB across the whole range.

## What the tiers cost, at 100 sandboxes

| tier     | idle CPU over 20 s | per sandbox per s | MemAvailable | on disk   |
| -------- | ------------------ | ----------------- | ------------ | --------- |
| Hot      | 813 ms (4.06%)     | 0.406 ms          | 477 MiB      | layer     |
| Frozen   | **0 ms (0.00%)**   | 0                 | 498 MiB      | layer     |
| Snapshot | 0 ms               | 0                 | **2878 MiB** | 1251 MiB  |

Transitions, all 100 sandboxes at once:

| transition         | total  | each    |
| ------------------ | ------ | ------- |
| hot → frozen       | 291 ms | 2.9 ms  |
| hot → snapshot     | 15.4 s | 154 ms  |
| snapshot → hot     | —      | 81–195 ms (7 samples, median 98) |

Snapshotting 100 sandboxes returned 2401 MiB — effectively all of the
2368 MiB they held. The node came back to its pre-sandbox state.

## Overcommit

Hot is bounded by RAM: 100 × 512 MiB sold against 3654 MiB physical is
**14.0x**.

Snapshot moves the bound to disk, at 12.5 MiB per sandbox. bm's 62 GB of
free disk holds roughly 5000 snapshotted sandboxes, around **700x** nominal
— of which about 100 can be awake at once, each reachable in ~100 ms.

That is the honest shape of the claim: forkd converts a memory limit into a
disk limit. It does not make 5000 sandboxes concurrently serveable; it makes
5000 of them *held* while 100 serve, and makes the switch cost ~100 ms.

## Why admission control exists

The same fill without it, on the same node:

| sandboxes | MemAvailable | per sandbox | swap        | PSI  |
| --------- | ------------ | ----------- | ----------- | ---- |
| 60        | 1014 MiB     | 23.1 MiB    | 404 MiB     | 2.5  |
| 100       | 579          | 18.2        | 1065        | 11.8 |
| 120       | 408          | 16.6        | 1550        | 15.6 |
| 140       | 402          | 14.3        | 2047 (full) | 52.6 |
| 160       | 310          | 20.7 (101 of 160 serving) | 2047 | 83.1 |

At 139 the node stopped answering SSH for about twenty minutes. The kernel
never OOM-killed anything — swap absorbed it and the machine simply became
unusable.

Two things worth keeping:

**Free memory alone would not have caught this.** Apparent cost per sandbox
*fell* from 22.8 to 14.3 MiB as swap absorbed the pressure. The node looked
like it was getting cheaper while it was getting slower. PSI is the signal
that tracks reality — the same conclusion the cold tier reached before it
was deleted.

**"It fits" and "it still works" are different numbers.** 125 fit; 100 work.
The master had no view of either, because the node reports free memory and
pressure on every poll and the master discarded both.

## Caveats

- One node. Nothing here measures a fleet.
- Idle CPU at 1000 sandboxes is extrapolated, not measured: bm cannot hold
  1000 hot. Frozen measured 0 at 100, so 1000 frozen should also be 0; 1000
  *hot* would be about 0.4 s/s, or 40% of a core, if 0.406 ms held linearly.
- `--network=host`, so ports are a node-global resource and sandboxes can
  reach each other's localhost. Network isolation is not implemented.
- The wake samples were taken one at a time. Concurrent wake under load is
  not measured.

## Tier latencies, re-measured under per-sandbox namespaces

Measured 2026-10-24 on the same host, driven through the API, after each
sandbox was given its own network namespace and a /30. Every number above
this section predates that change.

The re-measurement was not a formality. Moving to namespaces broke the
restore path: gVisor takes the veth device over, and deleting the
container left it in the namespace with its address gone, so a restored
sandbox came back running but unreachable. Every restore now reapplies
the address first, and `snapshot -> hot` below is the proof.

| transition | with `--network=host` | with its own namespace |
| ---------- | --------------------- | ---------------------- |
| create → serving   | 207 ms | **238 ms** |
| hot → frozen       | 0 µs of CPU over 3 s | **0 µs**, unchanged |
| frozen → hot       | 18 ms  | **17 ms** |
| hot → snapshot     | 19 ms  | **35 ms** |
| snapshot → hot     | 83 ms  | **84 ms** |
| hot → detached     | 527 ms | **387 ms** |
| detached → hot     | 140 ms | **233 ms** |

Creating is ~31 ms slower, which is the namespace and veth being built.
Coming back from detached is ~93 ms slower for the same reason, since a
cold start builds the bundle from scratch. Everything else is within
noise of where it was, and freezing is still free rather than merely
cheap.

Also confirmed in the same run, on 30 of 30 checks with nothing above
INFO: the sandbox keeps its address across a snapshot round trip, the
template's `memory_max_mib` reaches the cgroup as `memory.max`, the
volume survives a detach while in-memory state correctly does not, and a
`kill -9` of the control plane leaves the sandbox serving and gets it
re-adopted rather than restarted.

Not re-measured: the fill curve and the 100-sandbox density figures. A
namespace and a veth pair per sandbox is work the old arrangement did
not do, so the ceiling is likely lower than 100 and the per-sandbox cost
higher than 23.7 MiB. Those numbers should not be quoted for a
namespaced node until they are taken again.
