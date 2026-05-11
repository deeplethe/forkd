# Week 1

The goal of week 1 is **a working snapshot/restore loop on a real Linux host**, with N=2 children sharing memory and observably running independent code.

No SDK. No fancy. Proof of life.

## Day 1 — Environment

On your Linux box:

- [ ] Run `scripts/setup-host.sh`
- [ ] Confirm `firecracker --version` works
- [ ] Confirm `ls -l /dev/kvm` is writable by your user
- [ ] Pull a kernel + rootfs from Firecracker quickstart
- [ ] Boot a vanilla microVM by hand: `firecracker --api-sock /tmp/fc.sock`
- [ ] SSH into it, run `uname -a`, shut it down

**Exit criteria**: you can boot, SSH into, and shut down a vanilla Firecracker microVM by hand.

## Day 2 — Snapshot/restore by hand

- [ ] Boot VM. Inside: `apt install python3`, `pip install numpy`.
- [ ] Pause via Firecracker API (`PATCH /vm` with `state: "Paused"`)
- [ ] Snapshot via API (`PUT /snapshot/create`): produces `vmstate` + `memory.bin`
- [ ] Restore (`PUT /snapshot/load`): confirm Python + numpy still there.

**Exit criteria**: snapshot a warm VM, restore it 30 seconds later, observe state preserved.

## Day 3 — Two children from one snapshot

- [ ] Restore twice in parallel from the same `vmstate` + `memory.bin`, into different work dirs (`/tmp/forkd-child-1`, `/tmp/forkd-child-2`).
- [ ] Verify both come up, both have independent vCPUs (different host PIDs).
- [ ] Run different commands in each: child 1 writes "A" to a file, child 2 writes "B".
- [ ] Check `/proc/<pid>/smaps` to confirm memory is `Shared_Clean` for the mmap region.

**Exit criteria**: two children alive from one snapshot. `smaps` proves CoW is happening.

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
