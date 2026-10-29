# Sandstorm against other open-source sandboxes, on bare metal — 2026-10-30

One machine, every runtime on it in turn, the same two programs, the same harness.

- **Machine:** 2 × Intel Xeon E5-2637 v2 (Ivy Bridge, 2013; 8 cores, 16 threads), 31 GB, a SATA SSD, Ubuntu 24.04, kernel 6.8, native KVM.
- **Programs:** `web`, a Python HTTP server, and `app64`, which holds a 64 MiB numpy heap and reads all of it on every request (`bench/fleet/apps`).
- **Start:** one instance at a time, from the call that starts it to its first 200, polled every 5 ms; 20 after one warm-up; p50.
- **Memory:** 100 instances kept running, each answering before the next starts; 30 s to settle; MemAvailable before and after, divided by the count; then 20 of them called at random. Nothing is tiered or paused in this table: Sandstorm ran with `--idle-frozen-secs 86400 --reclaim-hot off --reclaim-frozen off`, so its sandboxes stayed hot like everyone else's.
- **Harness:** [`compare.py`](../../compare.py), [`setup.sh`](../../setup.sh), [`fork.py`](../../fork.py), [`fork_msb.py`](../../fork_msb.py), [`fork_cube.py`](../../fork_cube.py). Raw rows: `run1.jsonl` (the runtimes that are not Sandstorm, plus microsandbox), `run2.jsonl` (Sandstorm), `run3.jsonl` (Kata), `run4.jsonl` (CubeSandbox), `fork.jsonl`.

## Start and memory

| | isolation | start, web / app64 | memory per instance, web / app64 |
|---|---|---:|---:|
| plain process | none | 67 / 200 ms | 7.8 / 79.7 MiB |
| bubblewrap | namespaces, host network | 74 / 195 ms | 8.3 / 66.9 MiB |
| Docker (runc) | namespaces | 256 / 414 ms | 15.2 / 88.5 MiB |
| Podman (crun) | namespaces | 296 / 429 ms | 10.8 / 84.0 MiB |
| Docker + gVisor | gVisor | 327 / 660 ms | 30.8 / 106.8 MiB |
| **Sandstorm, gVisor** | gVisor | **308 / 624 ms** | **24.0 / 97.2 MiB** |
| **Sandstorm, gVisor, warm template** | gVisor | **168 / 186 ms** | 24.3 / 97.8 MiB |
| Kata 4.2 (runtime-rs, QEMU) | VM | 3,005 / 3,397 ms | 223 / 311 MiB¹ |
| microsandbox 0.7.8 (libkrun) | VM | 1,117 / 1,119 ms | 80.2 / 166.2 MiB |
| CubeSandbox 0.7.3 | VM | **72 / 78 ms** | **8.9 / 12.2 MiB** |
| **Sandstorm, Firecracker** | VM | 636 / 964 ms² | 80.0 / 166.1 MiB |
| **Sandstorm, Firecracker, warm template** | VM | 129 / 199 ms | 10.8 / 11.9 MiB |

Every runtime answered 20 of 20 when sampled.

¹ Kata keeps every guest's memory in shared memory for virtio-fs, so `/dev/shm` (16 GB here) is its ceiling: 81 `web` guests, 57 `app64`. Past it a new guest fails to start, and at 57 `app64` none of the 20 sampled answered; its figure is from 40.
² After the fixes in deeplethe/sandstorm#380, each measured on its own: 1,953 / 2,120 ms before them. `run2.jsonl` has 868 / 1,174 ms, measured before the last two (`quiet`, the agent starting the program first).

Every CubeSandbox and warm Sandstorm sandbox starts from a snapshot of the program already running, and maps that memory privately, so a page only costs a sandbox once it writes it. `app64` only reads its heap after starting, which is why it costs those two barely more than `web`; a program that rewrote its heap would cost more.

## Snapshot and fork

The source is a running `app64` started from a template; polled every 5 ms.

| p50 | Sandstorm, Firecracker | Sandstorm, gVisor | CubeSandbox | microsandbox³ |
|---|---:|---:|---:|---:|
| snapshot a running sandbox | **52 ms** | 368 ms | 252 ms | — |
| start 1 from the snapshot, to its first answer | 135 ms | 138 ms | 133 ms | — |
| clone 1: snapshot, then start | **~187 ms** | ~506 ms | 542 ms | 658 ms |
| clone 10, until all answer | ~883 ms | ~1,530 ms | 973 ms | 835 ms |
| clone 20, until all answer | ~1,562 ms | ~2,760 ms | **1,046 ms** | 1,152 ms |

Sandstorm's clone is `branch` then one fork call with `n`, so the sum is approximate. CubeSandbox's is its SDK's `clone(n, concurrency=n)`: a snapshot, then n creates at once (0.7.3 has no `/fork`).
³ `msb fork` copies a running sandbox in one step. Its children get no port forward and cannot be called from the host, so its time is until `msb fork` returns with them running, not until they answer; each was checked from inside afterwards.

## What it says

- **Against the other VM runtimes,** a warm Sandstorm Firecracker sandbox starts 8× faster than microsandbox and 23× faster than Kata, in a seventh to a twentieth of the memory. Cold, it is still faster than both.
- **Against Docker with gVisor,** the same isolation, Sandstorm on gVisor starts a little faster cold, twice as fast warm, and holds 10-20% less per instance.
- **Against CubeSandbox,** Sandstorm snapshots a running sandbox 5× faster and clones one 3× faster. Starting one from a snapshot is level, and so is memory. CubeSandbox starts a sandbox from its template faster (72 against 129 ms for `web`, 78 against 199 for `app64`) and clones 20 faster (1,046 against ~1,562 ms).
- On the 2025 EPYC in the earlier bm run, Sandstorm's warm `web` start was 52 ms against CubeSandbox's 55. On this 2013 Xeon it is 129 against 72: something on Sandstorm's warm path is far more sensitive to an old CPU than CubeSandbox's, not yet found.
- A fork of n children is created one after another on the node, about 65 ms each; CubeSandbox and microsandbox create them in parallel.

## Found and fixed on the way

All in deeplethe/sandstorm#380:

- The gateway retried a sandbox that had not started listening yet with a backoff up to 500 ms, holding a first answer 300-370 ms past the program listening. Sandstorm gVisor cold `app64`: 941 → 616 ms.
- Firecracker cold start: i8042 probing, the agent importing numpy before starting the program, a SYN resent only after 1 s, and above all ARP for a guest still booting asked again only after 1 s; then `quiet` and the program started first. `web` 1,953 → 636 ms.
- A fixed tap MAC, without which pinning the guest's neighbour entry made every warm start 43 s.
- A guest hostname that resolves without DNS, which cost 20 s per Firecracker sandbox on bm.
- Two master-and-node processes on one machine sharing gateway ports: the second now refuses to start.

## Not covered

- E2B self-hosted (Nomad and Consul) was not set up.
- No load: idle sandboxes that each answered single requests.
- One machine, one run of each; the p90s are in the raw rows.
- bubblewrap shares the host's network here, so it did the least work of all.
