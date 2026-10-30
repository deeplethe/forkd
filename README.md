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

**One machine, thousands of sandboxes.** Sandstorm pauses idle sandboxes and snapshots them to disk, then restores them quickly when a request arrives, so one machine can hold far more sandboxes than it could run at once. It supports two sandbox backends, [gVisor](https://gvisor.dev) containers and [Firecracker](https://firecracker-microvm.github.io) microVMs. Every sandbox has its own network namespace, and both backends can run in one fleet, each forming a resource pool of its own. The master serves a web console for viewing and managing sandboxes and scheduling across the fleet.

<div align="center">

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/charts/compare-dark.svg">
  <img alt="Start time, memory and clone time of a 64 MiB app, by runtime: on a cloud VM, Sandstorm on Firecracker from a warm template 84 ms and CubeSandbox 87 ms; on a KVM machine 100 ms and 129 ms, in 11 and 12 MiB, and 938 ms and 1.1 s to clone into 20; Docker with gVisor 685 ms, microsandbox 1.1 s and Kata 3.4 s." src="docs/assets/charts/compare-light.svg" width="100%">
</picture>

</div>

<sub>FC: Firecracker. Cube: CubeSandbox. +gVisor: Docker with gVisor. Cloud VM: six 2-core, 2 GB nodes for gVisor and containers; one 2-core, 3.6 GB VM under PVM for Firecracker and CubeSandbox. KVM machine: 2 × Xeon E5-2637 v2, 31 GB. Warm: restored from an image of the app already started. Clones are of a running app, until all 20 answer (microsandbox: until running).</sub>

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

The master serves a console at `/`: where every sandbox is, how fast they move between tiers, and each node's headroom.

<p align="center">
<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/console-dark.webp">
  <img alt="The Sandstorm console: counts of sandboxes, awake ones, nodes, creates per minute and API p99 across the top; the tier flow between awake, frozen, on disk and detached; sandboxes by tier over time, lifecycle rates, node headroom and latency." src="docs/assets/console-light.webp">
</picture>
</p>

## Latency

Six 2-core, 2 GB cloud nodes on gVisor, no KVM, 2026-10-30. Requests from each node to its own gateway, so no network is in the path.

| | |
|---|---:|
| Wake from frozen, 64 MiB app | 100-170 ms |
| Wake from disk, 16 / 64 / 256 MiB app | 130 / 150 / 750 ms |
| Wake from disk, 512 MiB / 1 GiB app | 1.1 / 3.3 s |
| Kept in memory per node: web / 64 MiB / 256 MiB app | 50 / 13 / 6 |

Under traffic, with 40 sandboxes per node where about 12 fit awake and a few getting most requests: at 1, 2, 4 and 8 requests a second per node, **1.3%, 0.2%, 0.1% and 1.3%** were refused, of 40,523. p50 is 6-8 ms; p99 is about 2 s, the requests that had to wait for a restore. A wake that cannot start within 2 s is refused rather than queued: a node restores two at a time and queues four, so of 10 or more woken at the same instant, six are answered and the rest get a 503 at once.

## Method

The figure at the top: every runtime started one at a time, polled every 5 ms, p50 of 20 ([`compare.py`](bench/baremetal/compare.py)). Sandstorm's time includes the master placing the sandbox and the node setting up its network. The cloud column is the median of six 2-core, 2 GB nodes, and for Firecracker and CubeSandbox one 2-core, 3.6 GB VM with CubeSandbox's PVM host kernel, which gives a VM without KVM a `/dev/kvm`.

A warm template restores an image of the app already started, as CubeSandbox always does; it is opt-in here, because every sandbox starts from the same memory and the template has to say how to reseed. CubeSandbox's API always restores a snapshot, so its cold figure comes from its own `cubemastercli multirun`, with a template's create request minus the snapshot annotations and a 5 ms probe: from the client's call until the program answers, as for the rest.

Under PVM the two warm starts are level: Sandstorm 52 / 83 ms, CubeSandbox 53 / 87 ms for web and the 64 MiB app. On the KVM machine Sandstorm's is ahead: 67 / 100 ms against 78 / 129 ms. Cold, Sandstorm leads on both: 545 / 921 ms against 828 / 999 ms on the KVM machine, 667 / 869 ms against 845 / 1,094 ms under PVM, where a guest boots without a serial console unless `--fc-serial-console` says otherwise. The 64 MiB app was 74 ms and 78 ms on the same machine a day earlier, with the same binary, so take that column as relative. A snapshot of a running sandbox took Sandstorm 30 ms and CubeSandbox 304 ms. Clones into 1, 10 and 20 took Sandstorm 138, 507 and 938 ms, and CubeSandbox 494, 1,046 and 1,064 ms; Sandstorm creates them one after another on the node, about 45 ms each, where CubeSandbox creates them at once, so a larger n would close that gap. E2B self-hosted was not set up, and memory under PVM was not measured. Raw rows: [bench/baremetal/results/2026-10-30](bench/baremetal/results/2026-10-30/README.md), [bench/fleet/results/2026-10-30](bench/fleet/results/2026-10-30/README.md).

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
