<div align="center">

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/logo-dark.svg">
  <img alt="Sandstorm" src="docs/logo.svg" width="383">
</picture>

</div>

<br/>

<div align="center">

[How it works](#how-it-works) · [Numbers](#numbers) · [Quick start](#quick-start) · [Status](#status)

[![CI](https://img.shields.io/github/actions/workflow/status/deeplethe/sandstorm/ci.yml?branch=dev&style=flat-square&label=CI&labelColor=161B22)](https://github.com/deeplethe/sandstorm/actions)
[![License](https://img.shields.io/badge/LICENSE-APACHE%202.0-3FB950?style=flat-square&labelColor=161B22)](LICENSE)
[![Rust](https://img.shields.io/badge/BUILT%20WITH-RUST-F74C00?style=flat-square&labelColor=161B22&logo=rust&logoColor=FFFFFF)](https://www.rust-lang.org)
[![DeepLethe](https://img.shields.io/badge/BUILT%20BY-DEEPLETHE-F1F2F5?style=flat-square&labelColor=161B22)](https://deeplethe.com)
[![gVisor](docs/assets/badges/gvisor.svg)](https://gvisor.dev)
[![Firecracker](docs/assets/badges/firecracker.svg)](https://firecracker-microvm.github.io)
[![中文](https://img.shields.io/badge/LANG-%E4%B8%AD%E6%96%87-DA3633?style=flat-square&labelColor=161B22)](README-zh.md)

</div>

**A sandbox fleet that is oversold on purpose.** Most agent sandboxes sit idle most of the time. Sandstorm pauses an idle sandbox, then checkpoints it to disk, and brings it back the moment a request reaches it. So a machine holds far more sandboxes than it could keep running. Every sandbox is a [gVisor](https://gvisor.dev) container with its own network namespace, and Sandstorm runs on ordinary cloud VMs: no KVM, no nested virtualisation. On a machine with KVM a node can run its sandboxes as [Firecracker](https://firecracker-microvm.github.io) microVMs instead (`sandstormd serve --node --backend firecracker`), which started faster than every other sandbox we measured ([below](#on-a-kvm-machine-against-the-other-sandboxes)). The backend is each node's own: one fleet can mix them, and a node on Firecracker forms a pool of its own, since a snapshot taken under one backend cannot be restored under the other; a create names the pool it wants.

On six cloud machines with 2 cores and 2 GB each, which hold **286** Python HTTP servers kept in memory, Sandstorm held **20,000** — and of 1,200 picked at random afterwards and called, 1,198 answered.

<div align="center">

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/charts/fleet-overview-dark.svg">
  <img alt="Four panels. On the left, capacity: 20,000 sandboxes on six 2-core, 2 GB machines that hold 286 kept in memory, and by heap size, how many a node keeps in memory as a sandbox and as a plain process, with the wake time from disk under each size. On the right, latency: by idle time, and under rising traffic against what round 3 refused" src="docs/assets/charts/fleet-overview-light.svg" width="800">
</picture>

</div>

The master serves a console at `/`: where every sandbox is, how fast they move between tiers, and each node's headroom. Here on a simulated six-node fleet.

<p align="center">
<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/console-dark.webp">
  <img alt="The Sandstorm console: counts of sandboxes, awake ones, nodes, creates per minute and API p99 across the top; the tier flow between awake, frozen, on disk and detached; sandboxes by tier over time, lifecycle rates, node headroom and latency." src="docs/assets/console-light.webp">
</picture>
</p>

## How it works

Every sandbox is in one of four tiers. It drops a tier at a time as it stays idle, and any request takes it straight back to hot.

| Tier | Where it is | Holds memory | Back to hot |
|---|---|---|---|
| **hot** | running | yes | — |
| **frozen** | paused in place (`cgroup.freeze`); its cold pages squeezed into zswap | some | ~10 ms |
| **snapshot** | checkpointed to the node's disk; its network namespace released | no | ~0.2 s |
| **detached** | only its volume is left | no | cold start |

- **One master, many nodes.** The master keeps desired state in SQLite and decides placement. The master connects to a node by address and pairing code, or a node behind NAT dials the master.
- **Wake on traffic.** A request to a sandbox's port reaches a gateway on its node, which wakes the sandbox and then forwards the request.
- **Nodes protect themselves.** A node short on memory turns work away instead of running out, and still answers ssh.
- **One binary.** `sandstormd` is the master, the node and the web console.

## Latency and cost

The other three panels of the figure at the top, the two on the right first:

- **Waking.** Timed on the node, so no network is in the path. A hot sandbox answers in 3.8 ms. Frozen, the thaw takes 11 ms and reading its heap back from zswap takes most of the rest: about 100 ms. On disk, the restore is about 230 ms for a 64 MiB heap.
- **Under traffic.** 40 sandboxes per node, where about 12 fit awake, with a few of them getting most requests. A wake that finds the node out of memory puts the sandboxes least in demand to disk to make room, and one that cannot start within 2 s is refused rather than queued, so at 1, 2, 4 and 8 requests a second per node **1.1%, 0.1%, 0.2% and 2.5%** of requests were refused, against 13%, 26%, 32% and 33% before (three runs, 40,500 requests). The cost is in the tail: a request that has to wait for a restore takes about 2 s at p99, at every rate. Past what a node can turn over — 80 per node at 8 req/s — it refuses 10.6%, with a p99 of 3.2 s.
- **Heavier sandboxes.** With a large heap, a sandbox costs about what the same program does as a plain process: 11 against 10 per node at 64 MiB, 4 against 3 at 256 MiB, 2 and 1 for both at 512 MiB and 1 GiB, because the node squeezes idle pages into zswap and the test heap compresses well. With a small one, gVisor's own overhead dominates, 19 against 24 at 16 MiB, which is what tiering makes up for. Waking from disk grows with the heap: 0.2 s at 64 MiB, 1 s at 256 and 512 MiB, 3.6 s at 1 GiB.

## Against the same machines without Sandstorm

<div align="center">

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/charts/fleet-compare-dark.svg">
  <img alt="Python HTTP servers one 2-core, 2 GB node holds: about 96 as plain processes, 94 under bubblewrap, 88 under Podman, 60 under Docker, 32 under Docker with gVisor, 48 in Sandstorm with every one awake, and 3,333 in Sandstorm with idle ones tiered" src="docs/assets/charts/fleet-compare-light.svg" width="800">
</picture>

</div>

The same Python HTTP server on the same six nodes, each runtime kept in memory until 512 MiB is left, twice on every node ([`baselines.py`](bench/fleet/baselines.py)). Sandstorm with every sandbox awake holds half again as many as Docker with gVisor, the same isolation, because it runs no daemon and no shim per container; with idle ones tiered it holds two orders of magnitude more than anything that keeps them in memory.

| Per node, 2 vCPU, 1.9 GB | HTTP server kept in memory | 64 MiB heap |
|---|---:|---:|
| plain process | 96 | 11 |
| bubblewrap | 94 | 11 |
| Podman (crun) | 88 | 12 |
| Docker (runc) | 60 | 10 |
| Docker + gVisor | 32 | 9 |
| **Sandstorm, all awake** | **48** | **13** |
| **Sandstorm, idle ones tiered** | **3,333** | |

Starting, on the same kind of machine: six 2-core, 2 GB cloud VMs (five AMD EPYC 7K62, one Intel Xeon 8255C), every runtime started one at a time and polled every 5 ms until it answers, 20 times, then the median of the six nodes' p50s ([`compare.py`](bench/baremetal/compare.py), [`setup-gvisor.sh`](bench/baremetal/setup-gvisor.sh)). Sandstorm's time includes the master placing the sandbox and the node setting up its network.

<div align="center">

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/charts/fleet-start-dark.svg">
  <img alt="Start to first answer on 2-core, 2 GB cloud machines, web server and 64 MiB app: Sandstorm from a warm template 150 and 192 ms, Sandstorm 242 and 380 ms, plain process 44 and 168 ms, bubblewrap 50 and 162 ms, Podman 195 and 358 ms, Docker 237 and 409 ms, Docker with gVisor 336 and 632 ms" src="docs/assets/charts/fleet-start-light.svg" width="800">
</picture>

</div>

| Start to first answer, p50 | web | 64 MiB app |
|---|---:|---:|
| plain process | 44 ms | 168 ms |
| bubblewrap | 50 ms | 162 ms |
| Podman (crun) | 195 ms | 358 ms |
| Docker (runc) | 237 ms | 409 ms |
| Docker + gVisor | 336 ms | 632 ms |
| **Sandstorm** | **242 ms** | **380 ms** |
| **Sandstorm, warm template** | **150 ms** | **192 ms** |

With the same isolation as Docker with gVisor, Sandstorm starts the 64 MiB app in 40% less time, about as fast as Docker without gVisor. A warm template restores an image of the app already started, so it skips Python importing numpy and filling its heap, and lands within 25 ms of the bare process; it is opt-in, because every sandbox starts from the same memory and the template has to say how to reseed. Raw rows per node: [bench/baremetal/results/2026-10-30/fleet](bench/baremetal/results/2026-10-30/fleet).

Checkpoint and restore with CRIU under Podman, the nearest standard counterpart to the snapshot tier, takes 0.3 s to checkpoint a 64 MiB app and 0.24 s from restore to first answer; Sandstorm restores the same app in 0.2 s. E2B, CubeSandbox and microsandbox are built on Firecracker and need KVM, which these VMs do not have; they are measured on a machine that has it below, and [docs/COMPARISON.md](docs/COMPARISON.md) covers the field.

## On a KVM machine, against the other sandboxes

<div align="center">

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/charts/baremetal-start-dark.svg">
  <img alt="The 64 MiB app on one KVM machine, start to first answer and memory per instance: Sandstorm on Firecracker from a warm template 74 ms and 10 MiB, CubeSandbox 78 ms and 12 MiB, Sandstorm on gVisor from a warm template 123 ms and 97 MiB, Sandstorm on gVisor 545 ms and 98 MiB, Docker 414 ms and 88 MiB, Docker with gVisor 660 ms and 107 MiB, microsandbox 1.1 s and 166 MiB, Kata 3.4 s and 310 MiB" src="docs/assets/charts/baremetal-start-light.svg" width="800">
</picture>

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/charts/baremetal-clone-dark.svg">
  <img alt="Cloning a running 64 MiB app into 1, 10 and 20 copies that answer: Sandstorm on Firecracker 137, 528 and 980 ms; CubeSandbox 542, 973 and 1,046 ms; microsandbox 658, 835 and 1,152 ms to running; Sandstorm on gVisor 458, 1,340 and 2,280 ms" src="docs/assets/charts/baremetal-clone-light.svg" width="800">
</picture>

</div>

The runtimes that need KVM, run on one machine that has it: 2 × Xeon E5-2637 v2, 31 GB, Ubuntu 24.04. Every runtime in turn, the same two programs and the same harness ([`compare.py`](bench/baremetal/compare.py)). There Sandstorm can also run each sandbox as a [Firecracker](https://firecracker-microvm.github.io) microVM (`--backend firecracker`), and from a warm template that is the fastest start measured, in about CubeSandbox's memory.

| p50 | web | 64 MiB app | memory, web / 64 MiB app |
|---|---:|---:|---:|
| **Sandstorm, Firecracker, warm template** | **62 ms** | **74 ms** | 9.9 / **10.5 MiB** |
| CubeSandbox 0.7.3 | 72 ms | 78 ms | **8.9** / 12.2 MiB |
| **Sandstorm, gVisor, warm template** | 107 ms | 123 ms | 23.3 / 97.3 MiB |
| **Sandstorm, gVisor** | 234 ms | 545 ms | 24.1 / 97.9 MiB |
| Docker + gVisor | 327 ms | 660 ms | 30.8 / 106.8 MiB |
| microsandbox 0.7.8 | 1,117 ms | 1,119 ms | 80.2 / 166.2 MiB |
| Kata 4.2 (QEMU) | 3,005 ms | 3,397 ms | 223 / 311 MiB |

Sandstorm on Firecracker snapshots a running sandbox in 27 ms, against CubeSandbox's 252, and a clone of one into n is that snapshot and one fork call. Its children are created one after another on the node, about 50 ms each, where CubeSandbox and microsandbox create theirs at once, so a larger n would close the gap. E2B self-hosted was not set up. Method, raw rows and what the runs turned up: [bench/baremetal/results/2026-10-30](bench/baremetal/results/2026-10-30/README.md).

## Numbers

Six nodes (AMD EPYC, 2 vCPU, 1.9 GB each) and one master (2 vCPU), runsc `release-20261019.0`, on 2026-10-28.

| | |
|---|---|
| 20,000 Python HTTP servers at 20/s | all 20,000 up with nothing retried by hand, twice |
| 100 of them per node, picked at random, called | 1,198 of 1,200 answered; p50 1.3 s, p99 2.8–5.5 s |
| Wake: frozen / from disk, 64 MiB app | 11 ms / 200 ms (Sandstorm's own time) |
| Wake 20 / 40 at once on one node | all of them; slowest 6 s / 13 s |
| Create to first answer, 64 MiB app | p50 1.4 s with five starting at once on a node, polled every 0.25 s; 0.29 s one at a time (above) |

Some caveats. The idle timers were shortened so a run fits in minutes. The test heap is `numpy.ones`, which compresses unusually well in zswap. 20,000 is where the fill stopped, not a ceiling. Method, raw data, every run, and the bugs the runs turned up are in [bench/fleet/results/2026-10-28](bench/fleet/results/2026-10-28/README.md); the earlier round is [2026-10-26](bench/fleet/results/2026-10-26/README.md).

## Quick start

You need Linux with cgroup v2, [`runsc`](https://gvisor.dev/docs/user_guide/install/) on the `PATH`, and Rust to build.

```bash
git clone https://github.com/deeplethe/sandstorm.git && cd sandstorm
cargo build --release -p sandstorm-core
sudo ./target/release/sandstormd serve --master --node
```

That runs a master and a node in one process, with the console at `https://127.0.0.1:8080`. The API token is in `/var/lib/sandstorm/pki/api-token`. A sandbox starts from a template, a rootfs plus an OCI spec registered with `POST /v1/templates`; `scripts/fleet.sh seed` shows the smallest one.

**A fleet** of machines you can ssh to:

```bash
scripts/fleet.sh up --master m1 --nodes n1,n2,n3 --listen 80
scripts/fleet.sh seed     # put a template on each node
scripts/fleet.sh verify   # start one sandbox and check it runs
```

**A node that is already installed** listens with `SANDSTORM_LISTEN=0.0.0.0:80`. On first start it writes a pairing code to `/var/lib/sandstorm/pki/node/pairing-code`. Give the master its address and that code, with the master's CA and API token from its `/var/lib/sandstorm/pki`:

```bash
curl --cacert ca.pem -H "Authorization: Bearer $(cat api-token)" -H 'Content-Type: application/json' \
  https://master:8080/v1/nodes -d '{"address": "10.0.0.5", "pairing_code": "<code>"}'
```

The code stays the same across restarts, and whoever holds it can run anything on that node. Keep it like a root password.

The HTTP API is documented in [docs/API.md](docs/API.md). To load-test a fleet, use [bench/fleet](bench/fleet/README.md).

## Status

v0.6 is not released yet, and it changes a lot: the backend moves from Firecracker microVMs to gVisor, and one host becomes a fleet. The last release, v0.5.3, is the Firecracker fork runtime; its README is [at that tag](https://github.com/deeplethe/sandstorm/tree/v0.5.3). The design decisions are recorded in [docs/adr](docs/adr/README.md).

Next, from what the fleet runs found:

- [x] Make room on wake: put the sandbox least in demand to disk instead of refusing the request.
- [x] Retry a create the node held back once memory frees, instead of parking it; and clear its error once it runs.
- [x] Clear away a sandbox that dies, network included, and restart it if it is still wanted.
- [ ] Reserve capacity on the master as it places, so a burst cannot overshoot a node's last report.
- [ ] Restore several snapshots in parallel on one node.

## License

[Apache-2.0](LICENSE)
