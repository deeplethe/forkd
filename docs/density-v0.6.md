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

## The fill curve, re-measured under namespaces

Same host, same method, 2026-10-24. The expectation was that a namespace
and a veth pair per sandbox would lower the ceiling and raise the
per-sandbox cost. Neither happened to any degree worth reporting.

| sandboxes | MemAvailable | per sandbox | swap    | PSI avg10 |
| --------- | ------------ | ----------- | ------- | --------- |
| 20        | 2317 MiB     | 21.9 MiB    | 465 MiB | 0.0       |
| 60        | 1351         | 23.4        | 496     | 1.0       |
| 100       | 444          | 23.1        | 588     | 6.6       |
| →         | —            | —           | —       | declined  |

Admission stopped it at 100 again, on the memory floor: `456 MiB free,
below the 512 MiB floor`. So the headline is unchanged: **100 sandboxes
at 23.2 MiB each, 14x nominal overcommit**, on a node that stays healthy.

| | `--network=host` | own namespace |
| --- | --- | --- |
| ceiling | 100 | **100** |
| per sandbox | 23.7 MiB | **23.2 MiB** |
| hot idle CPU, 100 sandboxes | 4.06% of a core | **4.97%** |
| per sandbox per second | 0.406 ms | **0.497 ms** |
| frozen idle CPU | 0 ms | **0 ms** |
| freeze all 100 | 291 ms | **299 ms** |
| snapshot all 100 | 15.4 s | **16.5 s** |
| memory returned by snapshotting | 2401 MiB | **2373 MiB** |
| PSI at the ceiling | 0.1 | **6.6** |

A namespace per sandbox costs about **22% more idle CPU** and nothing
measurable in memory. Freezing is still free rather than merely cheap,
which is the property the whole design leans on.

The one number that moved the wrong way is pressure at the ceiling: 6.6
against 0.1. The node works harder to hold the same 100 sandboxes,
consistent with the extra CPU, and it is still inside the 8.0 admission
ceiling.

Also confirmed: the template's `memory_max_mib` reaches the kernel, with
8 of 8 sampled cgroups reporting `memory.max = 256 MiB`.

## The ~1 s wake was a measurement bug, and it corrected two numbers

That run reported `snapshot -> hot` at ~1024 ms, uniformly across eight
samples, against 84 ms for a single sandbox. It was the harness, not
forkd, and chasing it corrected a second figure in the other direction.

Instrumenting both ends showed the command path is fast: 17 ms from the
`touch` to the node receiving `SetTier`, 7 ms to reapply the address, and
`restored` 87 ms after the touch. Measured with an HTTP probe, **touch to
answering is 92-128 ms**, which agrees with the single-sandbox figure.

The clock was starting too early. Both harnesses waited for the snapshot
image *directory* to exist and began timing there, and `checkpoint()`
does `create_dir_all` before it runs `runsc checkpoint`. Measured over
three rounds, the directory appears **1013-1014 ms before the sandbox
actually stops answering**, so the reported wake was the tail of the
preceding snapshot plus the wake.

The same mistake read the other way had been flattering the snapshot
cost, which was timed the same way:

| | reported | actual |
| --- | --- | --- |
| snapshot → hot | ~1024 ms | **92-128 ms** |
| hot → snapshot | 35 ms | **~1050 ms** |

ARP was the first hypothesis and was wrong, which is worth recording
because it is what anyone would reach for: the sandbox-side MAC is
unchanged across a restore and flushing the neighbour cache makes no
difference, 1047 ms stale against 1052 ms flushed.

**Still unattributed:** `runsc checkpoint` is 42 ms while the whole
`hot -> snapshot` transition is ~1050 ms. Tracked in #326, scoped to the
snapshot path rather than the wake path.

A directory the code creates *before* doing the work is not a signal that
the work is done. Both harnesses now wait for the sandbox to stop
answering.
