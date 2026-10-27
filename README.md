<div align="center">

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/logo-dark.svg">
  <img alt="Sandstorm" src="docs/logo.svg" width="330">
</picture>

</div>

<br/>

<div align="center">

[How it works](#how-it-works) · [Numbers](#numbers) · [Quick start](#quick-start) · [Status](#status)

[![CI](https://img.shields.io/github/actions/workflow/status/deeplethe/sandstorm/ci.yml?branch=dev&style=flat-square&label=CI&labelColor=161B22)](https://github.com/deeplethe/sandstorm/actions)
[![License](https://img.shields.io/badge/LICENSE-APACHE%202.0-3FB950?style=flat-square&labelColor=161B22)](LICENSE)
[![Rust](https://img.shields.io/badge/BUILT%20WITH-RUST-F74C00?style=flat-square&labelColor=161B22&logo=rust&logoColor=FFFFFF)](https://www.rust-lang.org)
[![gVisor](https://img.shields.io/badge/ISOLATION-GVISOR-2D333B?style=flat-square&labelColor=161B22)](https://gvisor.dev)
[![中文](https://img.shields.io/badge/LANG-%E4%B8%AD%E6%96%87-DA3633?style=flat-square&labelColor=161B22)](README-zh.md)

</div>

**A sandbox fleet that is oversold on purpose.** Most agent sandboxes sit idle most of the time. sandstorm pauses an idle sandbox, then checkpoints it to disk, and brings it back the moment a request reaches it. So a machine holds far more sandboxes than it could keep running. Every sandbox is a [gVisor](https://gvisor.dev) container with its own network namespace, and sandstorm runs on ordinary cloud VMs: no KVM, no nested virtualisation.

On six cloud machines with 2 cores and 2 GB each, which hold **431** sandboxes kept in memory, sandstorm ran **18,290**.

<div align="center">

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/charts/fleet-running-dark.svg">
  <img alt="18,290 sandboxes running on six 2-core, 2 GB machines that hold 431 kept in memory" src="docs/assets/charts/fleet-running-light.svg" width="800">
</picture>

</div>

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
- **One binary.** `sandstormd` is the master, the node and the web panel.

## What a request waits

Measured on the node, so no network is in the path. A sandbox that is hot answers in 3.5 ms. Frozen, the thaw takes 10 ms and reading its heap back from zswap takes most of the rest. On disk, the restore is about 200 ms for a 64 MiB heap.

<div align="center">

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/charts/fleet-sleep-dark.svg">
  <img alt="Request latency by idle time: 3.5 ms hot, about 95 ms frozen, about 210 ms from disk" src="docs/assets/charts/fleet-sleep-light.svg" width="800">
</picture>

</div>

## Under traffic

40 sandboxes per node, where about 12 fit awake, with requests spread the way real use is: a few sandboxes get most of them. The median request stays at a hot sandbox's 3.4 ms. **What does not hold up is the tail:** waking a sandbox needs memory, and when the node has none to spare it refuses the wake rather than putting an idle sandbox to sleep to make room. 13% of requests were refused at 5 req/s, a third at 40.

<div align="center">

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/charts/fleet-mixed-dark.svg">
  <img alt="Median and p99 latency under rising traffic, with the share of requests refused per phase" src="docs/assets/charts/fleet-mixed-light.svg" width="800">
</picture>

</div>

## Heavier sandboxes

With a large heap, a sandbox costs about what the same program does as a plain process. With a small one, gVisor's own overhead dominates: an idle Python fits 314 times per node as plain processes and 72 times as sandboxes, which is what tiering makes up for. What grows with the heap is the time to bring a sandbox back from disk: 0.2 s at 64 MiB, 2.6 s at 512 MiB, 3.9 s at 1 GiB.

<div align="center">

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/charts/fleet-sizes-dark.svg">
  <img alt="Sandboxes kept in memory per node, and wake time from disk, for heaps from 16 MiB to 1 GiB" src="docs/assets/charts/fleet-sizes-light.svg" width="800">
</picture>

</div>

## Numbers

Six nodes (AMD EPYC, 2 vCPU, 1.9 GB each) and one master (2 vCPU), runsc `release-20261019.0`, on 2026-10-26.

| An idle Python sandbox, per node | |
|---|---:|
| as a plain process, no sandbox | 314 |
| as a gVisor sandbox, kept in memory | 72 |
| as a sandstorm sandbox, idle ones tiered down | **3,048** |

| | |
|---|---|
| Create to first answer, 64 MiB app | p50 1.3 s |
| Wake: frozen / from disk, 64 MiB app | 10 ms / 200 ms (sandstorm's own time) |
| Wake 10 at once on one node | all 10, slowest 2.8 s |
| Create requests the master accepts | ~500/s |
| Master | 16 MiB RSS, store lock p99 0.5 ms |

Some caveats. The idle timers were shortened so a run fits in minutes. The test heap is `numpy.ones`, which compresses unusually well in zswap. The 18,290 was not a ceiling: memory was still free when the ramp ended. It needs #351 and #352, which are not merged yet. Method, raw data and every run are in [bench/fleet/results/2026-10-26](bench/fleet/results/2026-10-26/README.md).

**Other sandboxes on the same machines.** E2B, CubeSandbox and microsandbox are built on Firecracker and need KVM, which these VMs do not have, so they could not be run here. [docs/COMPARISON.md](docs/COMPARISON.md) covers the field.

## Quick start

You need Linux with cgroup v2, [`runsc`](https://gvisor.dev/docs/user_guide/install/) on the `PATH`, and Rust to build.

```bash
git clone https://github.com/deeplethe/sandstorm.git && cd sandstorm
cargo build --release -p sandstorm-core
sudo ./target/release/sandstormd serve --master --node
```

That runs a master and a node in one process, with the panel at `https://127.0.0.1:8080`. The API token is in `/var/lib/sandstorm/pki/api-token`. A sandbox starts from a template, a rootfs plus an OCI spec registered with `POST /v1/templates`; `scripts/fleet.sh seed` shows the smallest one.

**A fleet** of machines you can ssh to:

```bash
scripts/fleet.sh up --master m1 --nodes n1,n2,n3 --listen 80
scripts/fleet.sh seed     # put a template on each node
scripts/fleet.sh verify   # start one sandbox and check it runs
```

**A node that is already installed** listens with `SANDSTORM_LISTEN=0.0.0.0:80`. On first start it writes a pairing code to `/var/lib/sandstorm/pki/node/pairing-code`. Give the master its address and that code:

```bash
curl --cacert ca.pem -H "Authorization: Bearer $SANDSTORM_TOKEN" \
  https://master:8080/v1/nodes -d '{"address": "10.0.0.5", "pairing_code": "<code>"}'
```

The code stays the same across restarts, and whoever holds it can run anything on that node. Keep it like a root password.

The HTTP API is documented in [docs/API.md](docs/API.md). To load-test a fleet, use [bench/fleet](bench/fleet/README.md).

## Status

v0.6 is not released yet, and it changes a lot: the backend moves from Firecracker microVMs to gVisor, and one host becomes a fleet. The last release, v0.5.3, is the Firecracker fork runtime; its README is [at that tag](https://github.com/deeplethe/sandstorm/tree/v0.5.3). The design is in [DESIGN-v0.6.md](DESIGN-v0.6.md) (Chinese).

Next, from what the fleet runs found:

- [ ] Make room on wake: put the least recently used sandbox to disk instead of refusing the request.
- [ ] Retry a create the node held back once memory frees, instead of parking it; and clear its error once it runs.
- [ ] Reserve capacity on the master as it places, so a burst cannot overshoot a node's last report.
- [ ] Restore several snapshots in parallel on one node.

## License

[Apache-2.0](LICENSE)
