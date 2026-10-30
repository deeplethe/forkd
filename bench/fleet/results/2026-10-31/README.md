# Latency on three hosts, 2026-10-31

The README's Latency table, measured under gVisor and under Firecracker on KVM and PVM, all on `dev` at `d58ab87`, the same release binary (`78165dce`). It has #400, #401 and #387, none of which the [2026-10-30](../2026-10-30/README.md) round had. Idle timers: frozen after 15 s, on disk after 60 s.

| | Backend | Machines |
|---|---|---|
| `gvisor/` | gVisor | six nodes, 2 cores, 2 GB, no KVM; master f1; rebooted first |
| `kvm/` | Firecracker v1.17.0 | one machine, 2 × Xeon E5-2637 v2, 16 threads, 31 GB, master and node in one process |
| `pvm/` | Firecracker v1.17.0 | bm, EPYC 9K65, 2 vCPU, 3.8 GB, CubeSandbox's PVM host kernel and guest kernel, master and node in one process |

The hosts differ, so read each column on its own. bm also held two other masters, idle, about 1.5 GB between them, which leaves it under 2.5 GB. Its traffic run overloaded it: it says what that host does, not what PVM does.

All templates are one ext4 image under Firecracker, with the guest memory the spec sets, and the same apps as gVisor.

## Wake

p50, 200 only.

| | gVisor | KVM | PVM |
|---|---:|---:|---:|
| Idle 1-10 s, not yet frozen, 64 MiB | 6 ms | 6 ms | 678 ms |
| Frozen, idle 18-50 s, 64 MiB | 103 ms | 165 ms (35-50 s) | 833 ms |
| On disk, idle 65-170 s, 64 MiB | 172 ms | 234 ms | 1.1 s |
| Restore, 16 / 64 / 256 MiB | 131 / 148 / 720 ms | 150 / 150 / 499 ms | 81 / 284 / 2,503 ms |
| Restore, 512 MiB / 1 GiB | 1,051 / 3,335 ms | 696 / 1,368 ms | 498 / 2,758 ms |

Samples: restores 60 per size under gVisor, 10 under Firecracker; 512 MiB and 1 GiB 6 under gVisor, **1** under Firecracker. On KVM the sandboxes idle 18 and 25 s had not been frozen yet, so frozen is 35-50 s. One of KVM's 84 idle probes failed.

The idle sweep is `sleep`: six nodes, two runs under gVisor; one node, six runs under Firecracker, for about as many samples.

## Under traffic

40 sandboxes per node, 64 MiB app, Zipf 1.1, open loop, 150 s at each rate, three runs. Refused, p50, p99.

| Per node | gVisor | KVM | PVM |
|---|---:|---:|---:|
| 1 req/s | 2.1%, 9.8 ms, 2.6 s | 0.2%, 50 ms, 1.9 s | 0%, 451 ms, 8.3 s |
| 2 req/s | 0.2%, 6.8 ms, 1.5 s | 0%, 8.5 ms, 794 ms | 0.2%, 724 ms, 20 s |
| 4 req/s | 0.1%, 6.1 ms, 1.7 s | 0%, 7.4 ms, 830 ms | 15%, 21 s, 58 s |
| 8 req/s | 1.6%, 7.3 ms, 2.4 s | 0%, 6.5 ms, 877 ms | 48%, 36 s, 60 s |

40,456, 6,737 and 6,838 requests.

## Herds

Of K woken at the same instant on one node, how many answered, mean of the runs.

| K | gVisor | KVM | PVM |
|---:|---:|---:|---:|
| 1 | 1 | 1 | 1 |
| 4 | 4 | 4 | 3.5 |
| 10 | 6 | 10 | 5 |
| 20 | 6 | 19.5 | 3.5 |
| 40 | 6 | 28 | 4 |

## Kept in memory, per node

| | gVisor | KVM | PVM |
|---|---:|---:|---:|
| web | 76 | 293 | 22 |
| 64 MiB app | 18 | 160 | 17 |
| 256 MiB app | 6 | 85 | 14 |

Two runs each, of 900 asked for; what was up after a 60 s settle (120 s for KVM's web and 64 MiB), per node. Not comparable with the 2026-10-30 figures, which allowed 10 s: since #387 a node counts what is still starting, and 10 s now shows 29 web sandboxes where 60 s shows 76. KVM stopped at its swap floor. PVM's figure is what fitted beside bm's other masters, and 256 MiB above 64 MiB is noise: 17 and 11 in its two runs.

## What went wrong on the way

- `workloads.py` waited for the disk by asking `runsc list`, so under Firecracker it returned at once and `sizes`, `heavy` and `herd` measured hot sandboxes: 5 ms restores. It now reads the sandbox's cgroup, and those three were rerun. `sizes`, `heavy` and `herd` here are the reruns.
- The `mixed80` runs were left out, or stopped: the README does not use them. Not in this directory.
- KVM's first walls were refused or left pending behind the last teardown. Rerun from an idle node.

`#405` changed `rootfs-init` after this build; these images have the older init.
