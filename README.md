# forkd

> Run 100 agent trajectories in parallel for the cost of one.

![status](https://img.shields.io/badge/status-pre--alpha-orange)
![license](https://img.shields.io/badge/license-Apache--2.0-blue)

**`forkd`** is a sandbox runtime that lets you fork a running microVM the same way you fork a process. Snapshot a parent VM with all your dependencies loaded, then spawn N children that share its memory copy-on-write.

Each child diverges only on the pages it writes — so 100 parallel agents cost a small fraction of running 100 independent sandboxes.

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

# 1. Snapshot a warm parent
forkd snapshot --tag demo \
    --kernel ./vmlinux-6.1.141 \
    --rootfs ./ubuntu-24.04.squashfs

# 2. Fork N children in parallel
forkd fork --tag demo --n 100
# ✓ all sockets up in 83 ms
# ✓ 100 restores fired in parallel in 119 ms
# ✓ total wall-clock: 202 ms
# ✓ 100 / 100 children alive
```

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
