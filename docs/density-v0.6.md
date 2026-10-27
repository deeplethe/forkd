# v0.6 density on one node

> **Everything below was first taken with `--network=host`, before each
> sandbox got its own network namespace.** Both the fill curve and the
> tier latencies have since been re-measured under namespaces; the two
> sections at the end carry the current numbers and the old ones are kept
> only for comparison. The 1000-sandbox idle figure remains an
> extrapolation.


Measured 2026-10-24 on `bm`: 2 vCPU, 3654 MiB RAM, 2047 MiB swap, 62 GB free
disk, ext4, gVisor runsc release-20261019.0. Driven entirely through the
sandstorm API; nothing in the harness touches runsc.

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

That is the honest shape of the claim: sandstorm converts a memory limit into a
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
| hot → snapshot     | 19 ms  | **~20 ms** (see below; the 35 ms timed the wrong event) |
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

## Two wrong numbers, and what the tiers actually cost

This section has been wrong twice, in both directions, and the cause was
the same each time: timing a tier change by asking the network instead of
the kernel. The sequence is worth keeping, because the final numbers are
only trustworthy once the way they were taken is on the record.

**First error.** `snapshot -> hot` was reported at ~1024 ms against 84 ms
for a single sandbox. The harness waited for the snapshot image
*directory* to exist and began timing there, and `checkpoint()` does
`create_dir_all` before it runs `runsc checkpoint`. So the "wake" was the
tail of the preceding snapshot plus the wake. Instrumenting both ends
showed the command path is fast — 17 ms to deliver `SetTier`, 7 ms to
reapply the address, `restored` 87 ms after the touch — and an HTTP probe
puts touch-to-answering at **92-128 ms**, agreeing with the 84 ms.

**Second error, which was mine correcting the first.** Having found that,
this document then claimed `hot -> snapshot` was ~1050 ms rather than
35 ms. That was worse than what it replaced. The 1050 ms was the probe's
own timeout: after the container is deleted its namespace still exists
with the veth up and nothing inside to send a RST, so a connect is
silently dropped and `curl` waits out its deadline. Varying only the
timeout proves it:

| `curl -m` | measured "stopped" |
| --------- | ------------------ |
| 1         | 1019 ms            |
| 0.5       | 520 ms             |
| 0.2       | 217 ms             |

**What the kernel says.** `cgroup.procs` empties when the sandbox's
processes are gone, which needs no probe and cannot be confused with a
dropped packet. Request to cgroup empty, three rounds: **19, 17, 23 ms**.
The node's own checkpoint span is 41-44 ms, which covers writing the
image after the tasks have already stopped.

So the ~1 second never existed in the product at any point. #326 is
closed on that basis.

| transition | honest figure | how it was taken |
| ---------- | ------------- | ---------------- |
| hot → snapshot | **~20 ms** to stop, ~42 ms to the image on disk | cgroup.procs emptying; the node's own log |
| snapshot → hot | **92-128 ms** | HTTP probe, touch to answering |

## A real wart found on the way

A connect to a sandbox that is not hot **hangs until the caller's own
timeout** rather than being refused: `curl -m 5` against a snapshotted
sandbox takes 5011 ms. The namespace outlives the container with its veth
up, so the SYN is dropped rather than rejected.

Nothing measured here depends on it, but a caller that reaches a cold
sandbox directly pays its full timeout to learn nothing, which is a poor
way to find out. Rejecting the port while a sandbox is not hot would turn
that into an immediate refusal.

## The lesson, stated once

Timing a tier change from outside, over the network, measures the probe as
much as the product. A directory the code creates before doing the work
is not a signal that the work is done, and a dropped SYN is not a signal
that a process has exited. Both harnesses now read `cgroup.procs`.

## Waking on traffic, and two levers the machine decides

Until now a cold sandbox's gateway port answered 503 and told the caller
to `POST /touch` to the master and retry. Every client of an exposed port
had to know about tiers, which is the asterisk on every density figure
above: a demoted sandbox was "available" only to callers who knew to wake
it.

**The gateway wakes it now.** A forward refused by #327's REJECT wakes the
sandbox on the node and forwards again; the caller gets a slower answer,
not an error, and nothing goes through the master (RFC §15.5). The node
reports the traffic, the master makes Hot the desired tier, and until it
has heard the node will not demote a sandbox that served a request in the
last 15 s. Responses that waited carry `x-sandstorm-woke-ms`.

Two more levers, both plain Linux and plain runsc, each probed at startup
and logged with the reason it is on or off:

- **Frozen reclaim** (`--reclaim-frozen auto`). After freezing, push the
  sandbox's memory into swap with `memory.reclaim`, with zswap writeback
  off for that cgroup so it stays compressed in RAM. On only where swap is
  zswap or zram: into a disk swap file the pages come back slower than a
  snapshot restores, so there it is left off.
- **Snapshot compression** (`--compress-snapshots auto`). Measures the
  disk's uncached read speed at startup and compresses where it is under
  500 MB/s, which is where a smaller image restores faster.

Measured on bm (2 vCPU, 3.6 GiB, 135 MB/s disk, zswap) with a 376 MiB
Python heap, 40 MiB of it random:

| | memory while cold | back to touching all of it |
| --- | --- | --- |
| frozen, as before | 376 MiB | 60 ms |
| frozen + reclaim (zswap, no writeback) | **113 MiB** | 310 ms |
| frozen + reclaim (writeback allowed) | 13-115 MiB | 0.4-3.9 s, unpredictable |

| snapshot image | size | restore, cold cache | restore, warm |
| --- | --- | --- | --- |
| uncompressed | 363 MiB | 3.3 s | 138 ms |
| flate-best-speed | **84 MiB** | **1.4 s** | 696 ms |

Tried and not adopted: `restore --background` returns sooner but the app
cannot run until the whole image is in (2.06 s either way);
`--exclude-committed-zero-pages` changed nothing, as gVisor already leaves
zero pages out; `--direct` made warm restores 3.2 s by bypassing the page
cache.

End to end through the gateway, a ~185 MiB heap, nothing calling `/touch`:

| wake from | memory while cold | first request | then a request touching the whole heap |
| --- | --- | --- | --- |
| frozen + reclaim | 70-88 MiB (hot: 171-184) | **24-34 ms** (wake 7-14) | 109-117 ms |
| snapshot (compressed) | 0, 57 MiB image on disk | **354-359 ms** (wake 341-346) | 28 ms |

Four rounds each, same process every time, and the master showed the
sandbox hot one second after each wake. That last figure needed its own
fix: a polling node's regular report can sit parked for a whole hold, and
until it arrived an explicit freeze matched the stale desired tier and did
nothing. The node now sends `POST /v1/node/traffic` the moment it wakes
something. A listening node is asked for a report every 5 s, which is its
window.

## Four more levers, and what the load-test fleet changed about them

All decided per node at startup, logged with the reason, and listed per
node on the master's `/metrics` as `sandstorm_node_feature`, because nodes in
one pool can come out differently.

**Compressed swap where there is none** (`--compressed-swap auto`). The
reclaim levers need swap in compressed RAM. The load-test fleet had a
2 GiB disk `/swap.img` on every node and zswap off, so both levers were
off everywhere. Now: disk swap with zswap off → zswap is turned on in
front of it; no swap at all → a zram device is added (via `hot_add`,
capped at a quarter of RAM by `mem_limit`); swap that is already
compressed → nothing. Either change is host-wide -- the host's own idle
pages can be compressed too -- and `--compressed-swap off` declines it.
Over zram, reclaimed pages skip zswap (`memory.zswap.max=0`) rather than
being compressed twice; and zram's pages are not charged to the sandbox's
cgroup, so per-sandbox memory reads low there (admission uses the host's
MemAvailable, which does not).

Both new routes measured on bm, a ~185 MiB heap frozen through the API:

| starting point | what sandstorm did | frozen cgroup | first request / whole heap |
| --- | --- | --- | --- |
| disk swap, zswap off (the fleet) | turned zswap on | 172-184 → 70-86 MiB | 21-27 / 99-108 ms |
| no swap at all | added zram0 (3.6 GiB logical) | 149-191 → 2 MiB, 56 MiB in zram | 25-28 / 204-209 ms |

**Hot reclaim** (`--reclaim-hot auto`). Senpai-style: every 6 s each hot
sandbox's cgroup is asked to give up a small fraction of what it holds,
growing while the sandbox does not stall on memory (0.1% of its time) and
reset when it does. The whole pass sits out while the host's PSI is above
half of `--admit-max-psi`: refaults count towards the PSI admission reads,
and reclaim must never be why a node refuses a sandbox. On bm, four hot
~185 MiB sandboxes each taking a light request every second:

| | total after 3 min | light request p50 / p99 | a request touching the whole heap |
| --- | --- | --- | --- |
| off | 748 MiB | 10 / 13 ms | 24-29 ms |
| on | **461-480 MiB** (still falling) | 10 / 13-15 ms | 85-91 ms |

Host PSI `some avg10` stayed at 0.00-0.02 throughout, against the 8.0
admission ceiling.

**Snapshot prefetch** (`--prefetch-snapshots auto`). A sandbox whose
traffic comes on a schedule -- three intervals within 25% of each other,
30 s or longer -- has its snapshot image read ~20 s before the next
predicted request. One sandbox called every 60 s, the page cache dropped
while it sat in Snapshot:

| | wake, cycles 4-7 |
| --- | --- |
| off | 1094-1112 ms |
| on | **841-942 ms** |

`drop_caches` also evicts runsc and the Python libraries, which prefetch
does not read; the 355 ms of a fully cached wake is the other end. Real
eviction is gentler than `drop_caches`, so this is the small end of it.

**Warm templates** (opt-in: `"warm": {"after_restore": [...]}`). New
sandboxes restore a booted image instead of booting. The image is taken
from a seed sandbox of the node's own, on the one network slot the
master's plan leaves out, so no caller's request state can be in it.
Every sandbox restored from one image starts with its memory, so the
template must say how to reseed: measured on bm, `os.urandom` is fresh in
each child but Python's `random`, OpenSSL's `RAND_bytes` and the string
hash seed are identical across a parent and its children. `after_restore`
runs inside each sandbox before it takes traffic; if it fails, that
sandbox boots cold instead. This is the contract of Lambda SnapStart and
CRaC, and it is why this one is not on by default.

On bm, a template that builds a ~185 MiB heap at startup, create to first
answer through the gateway:

| | create → first answer |
| --- | --- |
| cold | 644-704 ms |
| warm | **215-241 ms**, with outliers of 549 and 1043 ms (restores of 457 and 923 ms in the node log) |

Taking the image cost 2.5 s once, in the background. With a reseed hook
of one signal, three warm children drew different `random` and `ssl`
values, as a cold one does.

**Snapshot compression** now keeps its decision inside ±100 MB/s of the
500 MB/s threshold, remembered in the work dir: the fleet's disks read
486, 494, 498 and 645 MB/s, right across a bare threshold. A pool can
still mix -- images are only restored on the node that wrote them, and
restore reads the format from the image -- and `/metrics` shows which.
## The Snapshot tier gives back its network namespace

The multi-node load test topped out at 7284 healthy sandboxes, about 17x
its hot capacity, and the limit was neither sandbox memory nor disk: it
was kernel slab. A snapshotted sandbox's process is gone, but it kept its
namespace and veth, ~0.4 MiB of unreclaimable kernel memory each; 1538 of
them held 694 MiB of slab on a 1.9 GiB node. The snapshot image itself was
3.2 MiB of disk.

Now reaching Snapshot takes the namespace and veth down and keeps the
slot: same index, addresses, gateway port and token. A restore rebuilds
them before `runsc restore`. One `unreachable` route under the whole
sandbox range replaces the per-sandbox REJECT for this tier: a sandbox
with a namespace has a more specific /30 route through its veth; one
without gets "no route to host" for anything sent to it -- which the
gateway reads as cold and wakes on -- instead of its address following
the default route off the machine.

On bm, 200 sandboxes of a small HTTP template, all demoted to Snapshot:

| | slab after → before | netns / veths | cold REJECT rules | wake p50 / p90, through the gateway |
| --- | --- | --- | --- | --- |
| before | **+65 MiB** (SUnreclaim +58) | 200 / 200 | 200 | 140 / 154 ms |
| after | **+3 MiB** (SUnreclaim +1) | 0 / 0 | 0 | 174 / 181 ms |

Rebuilding the network costs ~34 ms of the wake: eight `ip` invocations.
Doing it over netlink directly would take most of that back.
