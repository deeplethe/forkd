#!/usr/bin/env bash
# create-initial-issues.sh — file the v0.0.1 issue backlog via `gh`.
#
# Run once, after `gh auth login`, against the deeplethe/forkd repo.
# Idempotent in spirit: if labels exist, gh prints a warning and continues.
#
# Each issue body is also captured in docs/initial-issues.md for reference.

set -euo pipefail

REPO="${REPO:-deeplethe/forkd}"

say() { printf "\033[1;34m==>\033[0m %s\n" "$*"; }

command -v gh >/dev/null || { echo "gh CLI not found"; exit 1; }
gh auth status >/dev/null 2>&1 || { echo "run: gh auth login"; exit 1; }

say "creating labels..."
gh label create -R "$REPO" "dirty-problem" --color "B60205" \
    --description "One of the 8 dirty problems in DESIGN.md" 2>/dev/null || true
gh label create -R "$REPO" "optimization"  --color "0E8A16" \
    --description "Performance / efficiency work" 2>/dev/null || true
gh label create -R "$REPO" "security"      --color "D93F0B" \
    --description "Security / isolation correctness" 2>/dev/null || true
gh label create -R "$REPO" "refactor"      --color "BFD4F2" \
    --description "Internal cleanup, no behavior change" 2>/dev/null || true

issue() {
    local title="$1" labels="$2" body="$3"
    say "filing: $title"
    gh issue create -R "$REPO" --title "$title" --label "$labels" --body "$body" \
        || echo "    (failed — may already exist; continuing)"
}

# ----------------------------------------------------------------------------

issue "MAC / IP hot-patch for restored children" \
"dirty-problem,help wanted" "$(cat <<'EOF'
## Problem
Every child restored from the same snapshot inherits the parent's MAC address. With N>1 children sharing a network this collides; even with isolated netns the guest kernel is confused if MAC changes silently mid-life.

Dirty problem #3 in [DESIGN.md](../blob/main/DESIGN.md).

## Approach (sketch)
- Parent boots with `macvtap` + a placeholder MAC.
- Guest runs a small daemon listening on `vsock`.
- On restore, host sends "new identity" message with new MAC/IP.
- Daemon triggers `systemd-networkd` reload.
- Target: < 10 ms per child after restore.

## Acceptance
- [ ] 100 children get 100 distinct MAC + IP combinations
- [ ] Guest correctly sees the new MAC (no link flap loop)
- [ ] Integration test
EOF
)"

issue "RNG re-seed on restore (security correctness)" \
"dirty-problem,security,help wanted" "$(cat <<'EOF'
## Problem
All children boot with parent's RNG state. **Cryptographically broken** — TLS sessions, key generation, anything entropy-dependent is predictable across children.

Dirty problem #4 in DESIGN.md.

## Approach
- At restore, host pulls fresh bytes from `/dev/urandom`.
- Send to guest via vsock (same channel as MAC patch, #1).
- Guest writes via `RNDADDENTROPY` ioctl.

## Acceptance
- [ ] Each child has different `/dev/urandom` output after restore
- [ ] Documented threat model (must call this out as required for production)
EOF
)"

issue "TSC offset randomization on restore" \
"enhancement,help wanted" "$(cat <<'EOF'
## Problem
All children resume with parent's TSC value. Identical wall-clock across children opens timing attacks and breaks log correlation.

## Approach
Firecracker exposes TSC config on snapshot/restore. Surface via `forkd-vmm::ForkOpts`.

## Acceptance
- [ ] `ForkOpts { randomize_tsc: true }` produces children with distinct TSC offsets
- [ ] Default = true
EOF
)"

issue "Per-child vsock CID allocator" \
"dirty-problem,help wanted" "$(cat <<'EOF'
## Problem
vsock CID is namespace-local but all N children from one snapshot start with the same CID. Either we avoid vsock (which blocks #1, #2) or we allocate.

Dirty problem #2 in DESIGN.md.

## Approach
- Per-host CID pool (start 1000, reuse on child death).
- Patch child's vmstate at restore to swap CID before resume.
- Stretch: virtio-net-only mode that sidesteps vsock entirely.

## Acceptance
- [ ] 100 children all reachable on distinct vsock CIDs
- [ ] CIDs reclaimed when child exits
EOF
)"

issue "KSM directed hints for fork families" \
"optimization,help wanted" "$(cat <<'EOF'
## Problem
Linux's default KSM scan is too lazy — minutes to reach steady-state sharing. We need seconds.

Dirty problem #6 in DESIGN.md.

## Approach
- `madvise(MADV_MERGEABLE)` on memory.bin VMA immediately after each restore.
- Tune `pages_to_scan` and `sleep_millisecs` based on family size.
- Stretch: kernel patch for fork-aware KSM (declare known-shared instead of scanning).

## Acceptance
- [ ] Within 1 s of restore, `Shared_Clean / Rss > 90%` for the memory.bin region
- [ ] Doesn't break existing KSM accounting
EOF
)"

issue "Hugepage-backed snapshot memory file" \
"optimization,help wanted" "$(cat <<'EOF'
## Problem
`memory.bin` on tmpfs invites OOM kill. On normal disk it's slow to mmap. Hugepages (2 MiB) reduce TLB pressure and avoid both.

Dirty problem #1 in DESIGN.md.

## Approach
- `memfd_create + MFD_HUGETLB` for snapshot memory file.
- `setup-host.sh` already reserves 512 hugepages; integrate.
- Benchmark vs current behavior.

## Acceptance
- [ ] Snapshot/restore works with hugepage-backed memory
- [ ] Measurable improvement in p99 restore latency
EOF
)"

issue "OOM protection: parent refcount" \
"dirty-problem,help wanted" "$(cat <<'EOF'
## Problem
If host hits memory pressure and OOM-kills the parent VM, **every child loses its CoW backing pages** and crashes.

Dirty problem #7 in DESIGN.md.

## Approach
- forkd-controller maintains reverse refcount: how many children depend on each parent's memory file.
- While refcount > 0, parent cgroup has `memory.swap.high` set high — push to swap rather than kill.
- Last child exit → unpin parent.

## Acceptance
- [ ] Stress test: fill host memory, parent survives, children survive
- [ ] Refcount visible via `forkd ls --verbose`
EOF
)"

issue "Per-child network namespace + macvtap setup" \
"enhancement,help wanted" "$(cat <<'EOF'
## Problem
Restored children currently share the host network namespace (effectively no isolation). Need per-child netns + tap + IP.

Day 4 work from WEEK1.md.

## Approach
- For each child: `ip netns add child-N`.
- macvtap inside the netns, attached to host iface.
- Configure firecracker `/network-interfaces/eth0` to use it.
- Combines with #1 (MAC patch) for full per-child identity.

## Acceptance
- [ ] `forkd fork --tag demo --n 10 --network` produces 10 children with 10 distinct IPs
- [ ] Each child can reach the internet through host NAT
EOF
)"

issue "Replace curl subprocess with hyper + hyperlocal" \
"refactor,good first issue" "$(cat <<'EOF'
## Problem
`forkd-vmm` shells out to `curl` for HTTP-over-unix-socket. Pragmatic for MVP, but adds ~10–20 ms per call and forks a subprocess each time.

## Approach
- Add `hyper` + `hyperlocal` to workspace deps.
- Wrap into `forkd-vmm/src/api.rs` `ApiClient`.
- Replace `api_call` callsites.
- Keep timeout knob via `tokio::time::timeout`.

## Acceptance
- [ ] All existing tests pass
- [ ] Per-call latency ≤ 1 ms (vs ~15 ms via curl)
- [ ] No regression in `forkd fork --tag demo --n 100`
EOF
)"

issue "ext4 rootfs builder script" \
"enhancement,good first issue" "$(cat <<'EOF'
## Problem
Firecracker quickstart rootfs is squashfs (read-only). We can't `apt install python3 numpy` to warm up state inside the parent. This blocks the killer demo: "100 Python sandboxes from one snapshot".

## Approach
- `scripts/build-rootfs.sh`: unsquashfs base, chroot + apt install, mkfs.ext4.
- Output: writable .ext4 rootfs, ~1 GiB.

## Acceptance
- [ ] `bash scripts/build-rootfs.sh ubuntu-24.04.squashfs python.ext4 "python3 python3-numpy"` works
- [ ] Booting the ext4: `apt list --installed` includes the requested packages
EOF
)"

issue "Python SDK skeleton" \
"enhancement,help wanted" "$(cat <<'EOF'
## Problem
Agent frameworks (LangGraph, CrewAI, AutoGen) are Python-first. forkd is a Rust binary. We need a Python package.

## Approach (MVP)
- `sdk/python/forkd/__init__.py`:
  - `class Parent` — wraps `forkd snapshot` via subprocess
  - `class Snapshot` — represents a tag
  - `class Children` — wraps `forkd fork`
- Future: gRPC client when controller has gRPC API.
- Mirror Modal's surface where it makes sense.

## Acceptance
- [ ] `pip install forkd` (from local path) works
- [ ] Example notebook forks 100 sandboxes and prints PIDs
EOF
)"

issue "Document the bash wait-on-firecracker gotcha" \
"documentation,good first issue" "$(cat <<'EOF'
## Problem
Found during Day 6 dev: bare `wait` in bash waits for **all** background children — including long-running firecracker processes themselves, which never exit. Hangs scripts indefinitely.

Fix: track only curl subshell PIDs and `wait $pid` per-PID.

## Approach
- Add a short section to `scripts/README.md` (create if needed) with the gotcha + example fix.
- Cross-reference the comment already in `day6-scale.sh`.

## Acceptance
- [ ] `scripts/README.md` exists and explains the trap
- [ ] Future bash work in `scripts/` avoids the same pattern
EOF
)"

echo
say "done. issue list:"
gh issue list -R "$REPO" --limit 20
