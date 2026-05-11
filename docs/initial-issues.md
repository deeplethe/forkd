# Initial issues for forkd v0.0.1

Issues to open against `deeplethe/forkd`. All written before we accept external contributions, so the front door is clean.

Eight map to the **dirty problems** from [DESIGN.md](../DESIGN.md). Four are project-level (refactor, docs, tooling).

A companion script `scripts/create-initial-issues.sh` files all of these via `gh issue create`.

---

## #1 — MAC / IP hot-patch for restored children   `dirty-problem`

### Problem
Today every child restored from the same snapshot inherits the parent's MAC address. With N > 1 children sharing a network this collides, and even with isolated netns the guest kernel is confused if MAC changes silently mid-life.

This is dirty problem #3 in [DESIGN.md](../DESIGN.md).

### Approach (sketch)
- Parent boots with `macvtap` + a placeholder MAC.
- Inside the guest, a small daemon (`forkd-guest-agent`?) listens on `vsock`.
- On restore, the host sends a "new identity" message with the new MAC/IP.
- Daemon triggers `systemd-networkd` reload.
- Target: < 10 ms per child after restore.

### Acceptance
- [ ] 100 children get 100 distinct MAC + IP combinations
- [ ] Guest correctly sees the new MAC (no link flap loop)
- [ ] Integration test in `scripts/dayX-network.sh`

### References
- DESIGN.md problem #3
- Firecracker network docs

---

## #2 — RNG re-seed on restore (security)   `dirty-problem`  `security`

### Problem
All children boot with the parent's RNG state. **Cryptographically broken** — TLS sessions, key generation, anything entropy-dependent is predictable across children.

This is dirty problem #4 in [DESIGN.md](../DESIGN.md).

### Approach
- At restore, host pulls fresh bytes from `/dev/urandom`.
- Send to guest via `vsock` (same channel as MAC patch, #1).
- Guest writes to `/dev/random` (or `RNDADDENTROPY` ioctl).

### Acceptance
- [ ] Each child has different `/dev/urandom` output after restore
- [ ] No measurable entropy depletion in steady state
- [ ] Documented threat model (must call this out as required for production)

### References
- DESIGN.md problem #4
- `getrandom(2)`, `random(4)`

---

## #3 — TSC offset randomization on restore   `enhancement`

### Problem
All children resume with parent's TSC value. Wall-clock looks identical across children → timing attacks, log correlation issues.

### Approach
Firecracker config already exposes a `track_dirty_pages` flag and snapshot options that allow setting TSC offset on restore. Surface this via `forkd-vmm::ForkOpts`.

### Acceptance
- [ ] `ForkOpts { randomize_tsc: true }` produces children with distinct TSC offsets
- [ ] Default to true (security)

---

## #4 — Per-child vsock CID allocator   `dirty-problem`

### Problem
vsock CID is namespace-local but all N children from one snapshot start with the same CID. Either we don't use vsock (limits #1, #2) or we allocate.

This is dirty problem #2 in [DESIGN.md](../DESIGN.md).

### Approach
- Maintain a per-host CID pool (start at 1000, reuse on child death).
- Patch the child's vmstate at restore time to swap CID before resume.
- (Stretch) virtio-net only mode that sidesteps vsock entirely.

### Acceptance
- [ ] 100 children all reachable on distinct vsock CIDs
- [ ] CIDs reclaimed when child exits

---

## #5 — KSM directed hints for fork families   `optimization`

### Problem
Linux's default KSM is too lazy — it scans the whole system, takes minutes to reach steady-state sharing. We need seconds.

This is dirty problem #6 in [DESIGN.md](../DESIGN.md).

### Approach
- Immediately after each child restore, `madvise(MADV_MERGEABLE)` on the memory.bin VMA.
- Tune `/sys/kernel/mm/ksm/pages_to_scan` and `sleep_millisecs` based on family size.
- (Stretch) kernel patch for "fork-aware KSM" — declare known-shared instead of scanning.

### Acceptance
- [ ] Within 1 s of restore, `Shared_Clean / Rss > 90%` for the memory.bin region
- [ ] Doesn't break existing KSM accounting

---

## #6 — Hugepage-backed memory image   `optimization`

### Problem
`memory.bin` on tmpfs invites OOM kill. On normal disk it's slow to mmap. Hugepages (2 MiB) reduce TLB pressure and avoid both issues.

This is dirty problem #1 in [DESIGN.md](../DESIGN.md).

### Approach
- `memfd_create + MFD_HUGETLB` for the snapshot memory file.
- Pre-reserve hugepages in `setup-host.sh`.
- Benchmark vs current behavior.

### Acceptance
- [ ] Snapshot/restore works with hugepage-backed memory
- [ ] Measurable improvement in p99 restore latency, or memory pressure resilience

---

## #7 — OOM protection: parent refcount   `dirty-problem`

### Problem
If host hits memory pressure and OOM-kills the parent VM, **every child loses its CoW backing pages** and crashes.

This is dirty problem #7 in [DESIGN.md](../DESIGN.md).

### Approach
- forkd-controller maintains a reverse refcount: how many living children depend on each parent's memory file.
- While refcount > 0, parent's cgroup has `memory.swap.high` set high — push to swap rather than kill.
- Last child exit → unpin parent.

### Acceptance
- [ ] Stress test: fill host memory, observe parent survives, children survive
- [ ] Refcount visible via `forkd ls --verbose`

---

## #8 — Per-child network namespace + macvtap   `enhancement`

### Problem
Currently restored children all share the host network namespace (effectively no isolation). Need per-child netns + tap device + IP.

This is Day 4 work from `WEEK1.md`.

### Approach
- For each child: `ip netns add child-N`
- `ip link add macvtap0 link <host-iface> ...` inside the netns
- Configure firecracker `/network-interfaces/eth0` to use it
- Combines with #1 (MAC patch) for full per-child identity

### Acceptance
- [ ] `forkd fork --tag demo --n 10 --network` produces 10 children with 10 distinct IPs
- [ ] Each child can reach the internet through host NAT

---

## #9 — Replace `curl` subprocess with hyper + hyperlocal   `refactor`  `good-first-issue`

### Problem
`forkd-vmm` currently shells out to `curl` for HTTP-over-unix-socket. Pragmatic for MVP but adds ~10–20 ms per call and forces a subprocess fork per API call.

### Approach
- Add `hyper` + `hyperlocal` to workspace dependencies
- Wrap into a small `ApiClient` struct in `forkd-vmm/src/api.rs`
- Replace all `api_call` / `api_call_with_timeout` callsites
- Keep the timeout knob; `tokio::time::timeout` wraps the future

### Acceptance
- [ ] All existing tests pass
- [ ] `cargo bench` (TBD) shows per-call latency ≤ 1 ms (vs ~15 ms with curl)
- [ ] No regression in `forkd fork --tag demo --n 100`

---

## #10 — ext4 rootfs builder script   `enhancement`  `good-first-issue`

### Problem
The Firecracker quickstart rootfs is squashfs (read-only). We can't `apt install python3 numpy` to warm up state inside the parent. This blocks the killer demo: "100 Python sandboxes from one snapshot."

### Approach
- `scripts/build-rootfs.sh`: takes a base squashfs, unsquashfs's it, runs `apt install` inside chroot, mkfs.ext4's the result.
- Output: a writable .ext4 rootfs sized for ~1 GiB.
- Document in `WEEK1.md` Day 5 follow-up.

### Acceptance
- [ ] `bash scripts/build-rootfs.sh ubuntu-24.04.squashfs python.ext4 "python3 python3-numpy"` works
- [ ] Booting with the ext4 image: `apt list --installed` includes the requested packages

---

## #11 — Python SDK skeleton   `enhancement`

### Problem
Agent frameworks (LangGraph, CrewAI, AutoGen) are Python-first. Right now forkd is a Rust binary. We need a Python package that wraps it.

### Approach (MVP)
- `sdk/python/forkd/__init__.py`:
  - `class Parent` — wraps `forkd snapshot` via subprocess
  - `class Snapshot` — represents a tag
  - `class Children` — wraps `forkd fork`
- (Future) gRPC client when controller has gRPC API
- Mirror Modal's surface where it makes sense; differ where ours is sharper

### Acceptance
- [ ] `pip install forkd` (from local path) works
- [ ] Example notebook fork 100 sandboxes and print PIDs

### References
- Modal Python SDK for surface inspiration

---

## #12 — Document the bash `wait` gotcha   `documentation`  `good-first-issue`

### Problem
Found during Day 6 dev: bare `wait` in bash waits for **all** background children — including the long-running firecracker processes themselves, which never exit on their own. This hangs scripts indefinitely.

The fix is to track only the curl subshell PIDs and `wait $pid` per-PID.

### Approach
- Add a short section to `scripts/README.md` (create if needed) with the gotcha.
- Cross-reference the comment already in `day6-scale.sh`.

### Acceptance
- [ ] `scripts/README.md` exists and mentions the trap
- [ ] Future bash work in `scripts/` avoids the same trap
