# forkd

> Run 100 agent trajectories in parallel for the cost of one.
> **Full Linux. Multi-vCPU. Networking. Real `apt install` and `curl`.**

![status](https://img.shields.io/badge/status-pre--alpha-orange)
![license](https://img.shields.io/badge/license-Apache--2.0-blue)

**`forkd`** is a sandbox runtime that lets you fork a **full-Linux microVM** the same way you fork a process. Snapshot a parent VM with your runtime + deps + state already loaded, then spawn N children that share its memory copy-on-write.

Each child is a real KVM virtual machine with multiple vCPUs, its own kernel, and (planned) its own network namespace — diverging from the parent only on the pages it writes. 100 parallel agents cost a small fraction of running 100 independent sandboxes.

This is the **OS-level** fork primitive. If you need single-function sub-ms sandbox calls (Python REPL, JS eval, stateless compute), see **[zeroboot](https://github.com/zerobootdev/zeroboot)** — different tradeoff point, complementary project.

## Why this exists

Agents today explore sequentially: try A → fail → rollback → try B. Tree search, self-consistency, parallel MCTS — beautiful in papers, untenable in production because every branch costs a full sandbox boot.

`forkd` makes branching cheap. Try 100 trajectories, kill 99, keep the one that works.

## Status

**Pre-alpha.** v0.1 prototype targeting Q3 2026. Star + watch for updates.

A hacker demo lands in 3–4 weeks: snapshot a Python+PyTorch parent, fork 100 children, run different prompts in parallel.

## Current UX (works today)

```bash
# On a Linux host with KVM, after `bash scripts/setup-host.sh`:
cargo build --release

# 0. Build a Python-warm rootfs (1.5 GiB ext4, ~2 min via Docker)
sudo bash scripts/build-rootfs.sh ubuntu:24.04 python-rootfs.ext4 1536 \
    python3 python3-numpy python3-pip

# 1. Snapshot a python-warmed parent
sudo bash scripts/day4-network.sh                 # one-time host tap setup
forkd snapshot --tag pyagent \
    --kernel ./vmlinux-6.1.141 \
    --rootfs ./python-rootfs.ext4 \
    --tap forkd-tap0
# Console: forkd: numpy 1.26.4 imported in PID 1 (/usr/bin/python3)
# snapshot took 3506 ms

# 2. Fork 100 truly-multi-tenant children (each in its own netns)
sudo bash scripts/netns-setup.sh 100              # one-time per host boot
sudo -E forkd fork --tag pyagent -n 100 --per-child-netns
# ✓ all sockets up in 93 ms
# ✓ 100 restores fired in parallel in 100 ms
# ✓ total wall-clock: 193 ms
# ✓ 100 / 100 children alive

# 3. Run commands or eval in any specific child
sudo forkd ping --child forkd-child-7
# {"pong": true, "numpy_version": "1.26.4", "pid": 1}

sudo forkd eval --child forkd-child-42 -- "numpy.zeros(100).sum()"
# 0.0   (~1 ms — reuses warmed numpy)

sudo forkd exec --child forkd-child-42 -- python3 -c "import numpy; print(numpy.eye(3))"
# (~100 ms — fresh subprocess re-imports)
```

**The headline**: 100 sandboxes restored with Python + numpy **already loaded in PID 1's memory**, each in its own network namespace, addressable independently, in **193 ms total wall-clock**.

Cold-boot alternatives (CubeSandbox, fresh Firecracker) need to additionally `import numpy` per VM, which alone costs ~300 ms per sandbox. forkd's `eval()` against the warmed interpreter is **~96× faster** than `commands.run('python3 -c "..."')` for the same numpy operation.

## Planned UX (coming)

```bash
# Build a parent from a Forkfile describing your guest userspace
forkd parent build ./Forkfile

# Fork with a per-child command (needs guest agent — Week 2)
forkd fork --tag demo --n 100 --cmd "python solve.py"
```

## How it works

1. Parent VM boots, loads your runtime + deps, pauses.
2. `forkd` captures `vmstate` + `memory.bin`.
3. To fork: `mmap(memory.bin, MAP_PRIVATE)` per child — kernel CoW: reads share, writes allocate new pages.
4. Per-child setup: fresh netns + MAC/IP, overlay rootfs, reseeded RNG, fresh TSC offset.
5. Memory cost ≈ parent + Σ(child dirty pages). Typically <10% per child for stateless workloads.

See [DESIGN.md](./DESIGN.md) for the architecture and the 8 dirty problems we solve.

## Benchmarks

Same dev box (Ubuntu 24.04, kernel 6.14, 20 vCPU, 30 GiB RAM, no patches), same task ("spawn N=100 parallel sandboxes ready to run code"):

| Backend | Wall-clock | Host memory | Notes |
|---|---|---|---|
| **forkd** | **201 ms** | **12 MiB** (~120 KB / sandbox) | snapshot fork, mmap(MAP_PRIVATE) CoW |
| Docker `run -d` | 144,630 ms | 426 MiB | image cached, parallel; bottlenecked at daemon |
| Firecracker (cold-boot, no snapshot) | 995 ms | 8,430 MiB | each VM boots its own kernel |

Reproduce: `bash bench/compare-vs-docker.sh 100` after `forkd snapshot --tag demo`.

**vs Docker**: forkd is ~720× faster on this task. Docker's parallelism is bottlenecked by daemon API serialization, not container creation itself.

**vs fresh Firecracker**: forkd is ~5× faster and uses ~700× less host memory. The snapshot/restore + mmap CoW model bypasses kernel boot AND shares warmed memory across children.

**vs zeroboot**: zeroboot's spawn primitive is ~2.5× faster (0.79 ms vs ~2 ms per fork) by sidestepping Firecracker process spawn entirely — direct KVM ioctl on a single-vCPU template. They trade off networking, multi-vCPU, and full Linux semantics for that latency. Both projects are open-source; pick the tradeoff that matches your workload.

## Related work

Open-source projects in the same space:

- **[zerobootdev/zeroboot](https://github.com/zerobootdev/zeroboot)** — sub-ms fork primitive optimized for **stateless function calls**. Single-vCPU, no networking, serial I/O only. The "open-source Lambda" approach. Faster than forkd on raw spawn (~0.79 ms). The right choice when your workload is a single Python or JS evaluation per fork.
- **forkd** (this project) — full-Linux microVM fork. Multi-vCPU, networking (planned), complete OS. Slightly slower spawn (~2 ms) but supports `apt install`, `curl`, running servers, full agent workloads.

Closed-source incumbents:

- **Modal Sandbox** — proprietary; sells per-sandbox forking as a service. forkd's positioning is "self-hosted Modal" for the OS-level fork.
- **Daytona** — single-sandbox cold-start optimized (27 ms). Not a fork primitive.
- **E2B** — sandbox-per-user, no parallel fork capability.

## Roadmap

- **Week 4** — Hacker demo + Twitter clip.
- **Month 4** — v0.1: stable API, LangGraph integration, real SWE-bench numbers.
- **Month 9** — v0.5: framework default-integrates, used in real workloads.
- **Month 12** — v1.0: LF Sandbox application, multi-host scheduling, security audit.

## Contributing

Pre-alpha; APIs change daily. If you want to help shape the design, open an issue with your use case before sending a PR.

## License

Apache 2.0. See [LICENSE](./LICENSE) and [NOTICE](./NOTICE).

---

Stewarded by [Deeplethe](https://deeplethe.com).
