<div align="center">

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/logo-dark.svg">
  <img alt="Sandstorm" src="docs/logo.svg" width="383">
</picture>

</div>

<br/>

<div align="center">

[How it works](#how-it-works) · [Latency](#latency) · [Quick start](#quick-start) · [Status](#status)

[![CI](https://img.shields.io/github/actions/workflow/status/deeplethe/sandstorm/ci.yml?branch=dev&style=flat-square&label=CI&labelColor=161B22)](https://github.com/deeplethe/sandstorm/actions)
[![License](https://img.shields.io/badge/LICENSE-APACHE%202.0-3FB950?style=flat-square&labelColor=161B22)](LICENSE)
[![Rust](https://img.shields.io/badge/BUILT%20WITH-RUST-F74C00?style=flat-square&labelColor=161B22&logo=rust&logoColor=FFFFFF)](https://www.rust-lang.org)
[![DeepLethe](https://img.shields.io/badge/BUILT%20BY-DEEPLETHE-F1F2F5?style=flat-square&labelColor=161B22)](https://deeplethe.com)
[![gVisor](docs/assets/badges/gvisor.svg)](https://gvisor.dev)
[![Firecracker](docs/assets/badges/firecracker.svg)](https://firecracker-microvm.github.io)
[![中文](https://img.shields.io/badge/LANG-%E4%B8%AD%E6%96%87-DA3633?style=flat-square&labelColor=161B22)](README-zh.md)

</div>

**A sandbox fleet that is oversold on purpose.** Most agent sandboxes sit idle most of the time. Sandstorm pauses an idle sandbox, then checkpoints it to disk, and brings it back the moment a request reaches it. So a machine holds far more sandboxes than it could keep running. Every sandbox is a [gVisor](https://gvisor.dev) container with its own network namespace, and Sandstorm runs on ordinary cloud VMs: no KVM, no nested virtualisation. On a machine with KVM a node can run its sandboxes as [Firecracker](https://firecracker-microvm.github.io) microVMs instead (`sandstormd serve --node --backend firecracker`), which started faster than every other sandbox we measured ([below](#on-a-kvm-machine-against-the-other-sandboxes)). The backend is each node's own: one fleet can mix them, and a node on Firecracker forms a pool of its own, since a snapshot taken under one backend cannot be restored under the other; a create names the pool it wants.

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

## Latency

Six 2-core, 2 GB cloud nodes on gVisor, no KVM, 2026-10-30. Requests from each node to its own gateway, so no network is in the path.

| | |
|---|---:|
| Wake from frozen, 64 MiB app | 100-170 ms |
| Wake from disk, 16 / 64 / 256 MiB app | 130 / 150 / 750 ms |
| Wake from disk, 512 MiB / 1 GiB app | 1.1 / 3.3 s |
| Kept in memory per node: web / 64 MiB / 256 MiB app | 50 / 13 / 6 |

Under traffic, with 40 sandboxes per node where about 12 fit awake and a few getting most requests: at 1, 2, 4 and 8 requests a second per node, **1.3%, 0.2%, 0.1% and 1.3%** were refused, of 40,523. p50 is 6-8 ms; p99 is about 2 s, the requests that had to wait for a restore. A wake that cannot start within 2 s is refused rather than queued: a node restores two at a time and queues four, so of 10 or more woken at the same instant, six are answered and the rest get a 503 at once.

## Starting

Every runtime on those same nodes, one at a time, polled every 5 ms: p50 of 20, then the median of six nodes ([`compare.py`](bench/baremetal/compare.py)). Sandstorm's time includes the master placing the sandbox and the node setting up its network.

| Start to first answer | web | 64 MiB app |
|---|---:|---:|
| plain process | 44 ms | 168 ms |
| bubblewrap | 50 ms | 162 ms |
| Podman (crun) | 195 ms | 358 ms |
| Docker (runc) | 237 ms | 409 ms |
| Docker + gVisor | 336 ms | 632 ms |
| **Sandstorm** | **242 ms** | **380 ms** |
| **Sandstorm, warm template** | **150 ms** | **192 ms** |

A warm template restores an image of the app already started. It is opt-in, because every sandbox starts from the same memory and the template has to say how to reseed.

## On a KVM machine, against the other sandboxes

<div align="center">

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/charts/baremetal-start-dark.svg">
  <img alt="The 64 MiB app on one KVM machine, start to first answer and memory per instance: Sandstorm on Firecracker from a warm template 74 ms and 10 MiB, CubeSandbox 78 ms and 12 MiB, Sandstorm on gVisor from a warm template 123 ms and 97 MiB, Sandstorm on gVisor 545 ms and 98 MiB, Docker 414 ms and 88 MiB, Docker with gVisor 660 ms and 107 MiB, microsandbox 1.1 s and 166 MiB, Kata 3.4 s and 310 MiB" src="docs/assets/charts/baremetal-start-light.svg" width="800">
</picture>

</div>

One machine, 2 × Xeon E5-2637 v2, 31 GB, every runtime in turn on the same two programs ([`compare.py`](bench/baremetal/compare.py)).

| Sandstorm on Firecracker, warm template, against | Sandstorm | CubeSandbox 0.7.3 |
|---|---:|---:|
| Start, web / 64 MiB app | **62 / 74 ms** | 72 / 78 ms |
| Memory per instance, web / 64 MiB app | 9.9 / **10.5 MiB** | **8.9** / 12.2 MiB |
| Snapshot a running sandbox | **27 ms** | 252 ms |
| Clone a running sandbox into 1 / 10 / 20 | **~137 / ~528 / ~980 ms** | 542 / 973 / 1,046 ms |

Sandstorm creates clones one after another on the node, about 50 ms each, where CubeSandbox creates them at once, so a larger n would close that gap. E2B self-hosted was not set up. Method and raw rows: [bench/baremetal/results/2026-10-30](bench/baremetal/results/2026-10-30/README.md), [bench/fleet/results/2026-10-30](bench/fleet/results/2026-10-30/README.md).

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
