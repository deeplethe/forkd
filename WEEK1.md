# Week 1

The goal of week 1 is **a working snapshot/restore loop on a real Linux host**, with N=2 children sharing memory and observably running independent code.

No SDK. No fancy. Proof of life.

## Progress (live)

| Day | Status | Headline |
|---|---|---|
| 1 — env + boot | ✅ done | Ubuntu 24.04 microVM boots, exit_code=0 |
| 2 — snapshot/restore | ✅ done (plumbing) | restore + resume in **28 ms** (vanilla Firecracker) |
| 3 — 2 children, 1 snapshot | ✅ done | **27 ms parallel**, Shared_Clean/Rss ≈ **89%**, CoW verified |
| 4 — per-child netns | ⏳ pending | (deferred — doesn't block the CoW story) |
| 5 — wrap as Rust binary | ✅ done | **`forkd fork --n 100` works**. 202 ms total (2× faster than bash) |
| 6 — N=10/50/100/200 | ✅ done | **100 VMs in 396 ms / 203 MiB / 252× compression** (bash); **202 ms** via Rust CLI |
| 7 — write up + issues | ⏳ | |

## Day 1 — Environment ✅

On your Linux box:

- [x] Run `scripts/setup-host.sh` (partial — apt deps deferred, gcc installed manually due to bzip2 dep conflict)
- [x] Confirm `firecracker --version` works
- [x] Confirm `ls -l /dev/kvm` is writable by your user (via kvm group)
- [x] Pull a kernel + rootfs from Firecracker quickstart
- [x] Boot a vanilla microVM by hand → see `scripts/day1-boot.sh`
- [x] Serial console boot to systemd ready → shutdown clean (skipped interactive SSH; not needed yet)

**Exit criteria met**: `firecracker exit_code=0`, full Ubuntu 24.04 boot logged.

## Day 2 — Snapshot/restore by hand ✅

- [ ] ~~Boot VM. Inside: `apt install python3`, `pip install numpy`.~~ deferred to Day 5 (needs ext4 rootfs)
- [x] Pause via Firecracker API (`PATCH /vm` with `state: "Paused"`)
- [x] Snapshot via API (`PUT /snapshot/create`): produces `vmstate` + `memory.bin`
- [x] Restore (`PUT /snapshot/load`) in a **fresh** firecracker process, resume cleanly

See `scripts/day2-snapshot.sh`. Measured: snapshot 3.3 s (I/O bound), **restore 28 ms**.

**Exit criteria met**: same VM state survives across firecracker process boundary.

## Day 5 — Wrap as Rust binary ✅

`forkd-vmm` now provides a real Rust API:

```rust
let vm = Vm::boot(&BootConfig::quickstart(kernel, rootfs, work_dir))?;
// ... warm up ...
vm.pause()?;
let snapshot = vm.snapshot_to(vmstate, memory)?;
vm.kill()?;

let result = snapshot.restore_many(100, &work_dir)?;
println!("{}ms wall-clock, {}/100 alive", result.spawn_ms + result.restore_ms,
         result.children.iter().filter(|c| c.is_alive()).count());
```

CLI on top:

```bash
forkd snapshot --tag demo --kernel ./vmlinux-6.1.141 --rootfs ./ubuntu-24.04.squashfs
forkd fork --tag demo --n 100
# ✓ all sockets up in 83 ms
# ✓ 100 restores fired in parallel in 119 ms
# ✓ total wall-clock: 202 ms
# ✓ 100 / 100 children alive
```

**Rust CLI is ~2× faster than bash** end-to-end (202 ms vs 396 ms) because
`std::thread` + `Command::spawn` are much lighter than shell `&` + curl
subshells. Restore phase is the same (both bottlenecked by Firecracker
kernel work); the spawn phase is where Rust wins.

HTTP-over-unix-socket is implemented by shelling out to `curl` — pragmatic
for the MVP. A future PR replaces it with hyper + hyperlocal.

Deferred items: ext4 rootfs + python warmup, guest agent for in-VM commands.

---

## Day 6 — Scale to N children ✅ (jumped ahead — Day 4 deferred)

See `scripts/day6-scale.sh`. Vanilla Firecracker, no KSM tuning, no patches.

| N | spawn | restore (parallel) | wall-clock | host mem Δ | Σ Rss | Σ Shared_Clean | compression |
|---|---|---|---|---|---|---|---|
|  10 |  38 ms |  35 ms |  73 ms | (noise) | 161 MiB | 152 MiB (94%) | ~32× |
|  50 |  89 ms |  43 ms | 132 ms |  81 MiB | 807 MiB | 761 MiB (94%) | **316×** |
| 100 | 307 ms |  89 ms | 396 ms | 203 MiB | 1614 MiB | 1523 MiB (94%) | **252×** |
| 200 | 588 ms | 170 ms | 758 ms | 977 MiB | 3231 MiB | 3048 MiB (94%) | **105×** |

Key findings:
- **Restore is sub-linear**: 50× the children, 3.3× the time.
- **Sharing ratio stays at 94%** as N grows — more children → more reuse of parent's read-only pages.
- **Private_Dirty grows linearly in N** (9 → 45 → 91 MiB) — that's where compression eventually breaks down. Today we're nowhere near it.
- **fd cost: ~33 fds per child**. At default ulimit 524288 system-wide, fd budget allows ~15000 simultaneous children before refusing. Memory budget allows similar.

### Bug found and fixed during Day 6

Unqualified `wait` in bash waits for **all** background children — including the long-running firecracker processes themselves, hanging the script forever. Fix: track curl subshell PIDs and `wait` only on those. Took ~10 minutes to diagnose via `pstree`. Filed-able lesson for the Rust port.

---

## Day 3 — Two children from one snapshot ✅

- [x] Restore twice in parallel from same `vmstate` + `memory.bin`.
- [x] Verify both come up, both have independent vCPUs (different host PIDs).
- [ ] ~~Run different commands in each~~ deferred (no SSH yet — same as Day 2's python warmup, blocked on ext4 rootfs + guest agent).
- [x] Check `/proc/<pid>/smaps` confirms `Shared_Clean ≈ Rss` for the memory.bin region.

See `scripts/day3-fork.sh`. Measured (vanilla Firecracker, no KSM, no patches):

- 2 children restored + resumed in **27 ms parallel**
- **Shared_Clean / Rss ≈ 89%** per child
- Private_Dirty: ~850 KB per child after 2 s idle
- Pss ≈ Rss / 2 (mathematical signature of CoW)

**Exit criteria exceeded**: CoW provably working at vanilla-Firecracker level. ~60× memory compression vs naive N independent VMs.

## Day 4 — Network namespace per child

- [ ] Fresh netns per child.
- [ ] `macvtap` (or `tap`) inside the netns.
- [ ] Different host-visible IPs.
- [ ] (MAC hot-patch noted but not solved this week.)

**Exit criteria**: each child reachable on its own IP from the host.

## Day 5 — Wrap it as a Rust binary

- [ ] In `crates/forkd-cli`: implement `snapshot` and `fork --n` subcommands.
- [ ] Wrap the hand-driven steps as Rust code in `forkd-vmm`.
- [ ] Goal: `forkd snapshot --tag demo && forkd fork --tag demo --n 2`.

**Exit criteria**: a Rust binary does what we did by hand on Days 2–4.

## Day 6 — Push to N=10

- [ ] Scale `--n 2` to `--n 10`. Watch what breaks.
- [ ] Likely failures: vsock CID collisions, IP exhaustion, KSM lag, file descriptor limits.
- [ ] Document each. File GitHub issues.

**Exit criteria**: 10 children alive simultaneously, even if 30% need manual restart.

## Day 7 — Write down what we learned

- [ ] Update `DESIGN.md` with reality vs plan.
- [ ] Open GitHub issues for each rough edge.
- [ ] Pick the demo workload: which Python task makes the 30-second video?
- [ ] (Optional) Tweet a screenshot.

**Exit criteria**: public repo with code, DESIGN updated, issues filed, momentum visible.

## What we are NOT doing in week 1

- Python SDK
- gRPC API
- Multiple parent images
- Production-grade isolation
- Anything pretty
- KSM tuning (week 2)
- RNG / TSC reseed (week 2)
- The MAC hot-patch daemon (week 2)
