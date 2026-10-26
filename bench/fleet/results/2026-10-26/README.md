# Fleet run, 2026-10-26

Seven cloud machines, one afternoon. The numbers on the front page come
from here.

## Setup

| | |
|---|---|
| Master | 1 × Intel Xeon Gold 6148, 2 vCPU, 1.9 GB |
| Nodes | 6 × AMD EPYC 7K62, 2 vCPU, 1.9 GB, kernel 7.0.0-14 |
| Runtime | runsc `release-20261019.0` |
| Build | `dev` with #347 (the master connects to nodes on :80), before #348 (frozen reclaim, snapshot compression) |
| Template | `py` from `scripts/fleet.sh seed`: a sleeping Python process, host `/usr` bound read-only, 128 MiB limit, about 14 MiB resident |
| Swap | a 2 GB file on disk on every node, zswap off |
| Admission | defaults: 512 MiB free floor, memory PSI 8.0 |
| Idle timers | shortened so a run fits in minutes: frozen after 10 s and snapshot after 40 s in round 1, 5 s and 15 s in round 2 (defaults are 30 s and 5 min) |

All runs drive the master's HTTP API with [`loadtest.py`](../../loadtest.py).
`raw/` has each run's report. `running.csv` is the running count through
`x1` and `x2` back to back, which the README's chart is drawn from
(`python3 bench/fleet/charts.py bench/fleet/results/2026-10-26 docs/assets/charts`).
`per_node.csv` is where the 7,284 sat at the end of `x2`.

## Results

| Run | What | Result |
|---|---|---|
| `t1_c*` | 200 creates at concurrency 1, 4, 16, 64 | 396, 508, 482, 444 accepted/s. Store lock wait p99 0.5 ms throughout |
| `t2_wall` | create with `--touch` until refused: every sandbox resident | 433 placed, 431 running, 72–73 per node |
| `t3_overcommit` | 1,800 at 3/s, left idle | 1,800 running, 0 refused, 0 failed, all in snapshot |
| `x1_r*` | sustained create rate, 5 → 40/s | 5/s and 10/s clean. 20/s: 252 of 1,776 held back by nodes. 40/s: nodes pinned at the free-memory floor, 2,288 held back |
| `x2_total` | 6,000 at 15/s, through refusals | **7,284 running** across the six nodes (1,162–1,318 each), 0 failed after the held-back ones were retried |
| `t5_cold` | create → first exec, one at a time | p50 0.24 s, max 0.27 s |
| `t4_restore` | snapshot → hot, one at a time | p50 0.19 s |
| `x3_herd_clean_300` | wake 300 snapshots at once | 300 of 300 in 7.26 s, p50 4.45 s, p99 6.24 s. Restores run one at a time per node, about 7/s each |
| `x4_abuse` | 3,000 creates as fast as possible | master accepted all 3,000 (420/s). Nodes started 406 and held the rest back. 42 ssh probes during the run, 0 failed |

## What limits it

**Memory per resident sandbox.** About 14 MiB each, most of it the gVisor
sentry rather than the app. At the 512 MiB floor that is the 72 per node in
`t2_wall`.

**Kernel memory per snapshotted sandbox.** A snapshotted sandbox frees its
process memory but keeps its network namespace and veth: about 0.4 MiB of
kernel slab each. One node at 1,538 sandboxes had 694 MiB of slab. That is
the 7,284 ceiling. Disk was not close: each snapshot is about 3.2 MiB.

**Create rate.** The master accepts about 500 creates a second, but a node
starts one sandbox at a time. Above 15–20 creates a second across six nodes,
the free-memory figure each node last reported is stale by the time the
placements land, and nodes hold work back instead. They stay responsive while
they do it, which is what `x4_abuse` checks.

**Burst wakes.** One restore at a time per node. 300 at once across six nodes
takes about 7 s.
