# Changelog

Notable changes per release. forkd follows [Semantic
Versioning](https://semver.org/spec/v2.0.0.html) once it reaches
1.0; until then, the minor version can break compatibility.

## Unreleased

### Guest kernel rebuild — closes #218 + #225

forkd's pre-v0.5.1 vmlinux was Linux **4.14.174** built 2021-07-14.
That kernel predated `CONFIG_HW_RANDOM_VIRTIO`, `CONFIG_RANDOM_TRUST_CPU`,
the `random.trust_cpu=` cmdline option (Linux 4.19+), and `CONFIG_VMGENID`
(Linux 5.20+). Symptom: `getrandom(2)` blocked forever inside a
restored guest because the CRNG never marked itself initialized —
anything that touched OpenSSL (`pip install`, `urllib.request` HTTPS,
`requests`) hung indefinitely.

**Replacement**: Linux **6.1.141** from Firecracker's CI bucket
(`firecracker-ci/v1.13/x86_64/vmlinux-6.1.141`, sha256
`b36a4a1b10f33b9cfdcde3d1a787d9c090556a3edb211cd06d1f3f9a6c7e8724`).
Verified end-to-end on the dev box on 2026-06-05:

| | before | after |
|---|---|---|
| `uname -r` | 4.14.174 | 6.1.141 |
| `/dev/hwrng` | absent | present (virtio-rng) |
| `getrandom(GRND_NONBLOCK)` | EAGAIN | returns 16 bytes immediately |
| `ssl.create_default_context()` | blocks forever | 0.01 s |
| `urllib.request.urlopen("https://pypi.org/…")` | blocks forever | 0.25 s |
| `pip install numpy==2.0.2` | exit 1, empty output | 21 s, exit 0 |
| **VMGENID on FC restore** | n/a | dmesg: `random: crng reseeded due to virtual machine fork` |

Two new helper scripts:
- [`scripts/install-guest-kernel.sh`](./scripts/install-guest-kernel.sh) — downloads the FC CI vmlinux into `/var/lib/forkd/kernels/vmlinux`; the recommended path.
- [`scripts/build-guest-kernel.sh`](./scripts/build-guest-kernel.sh) — clones Linux 6.1 LTS, applies FC's reference microvm config, builds vmlinux from source (~10 min on a 20-core box). Use this when you want to audit / tune the kernel config or target a non-x86_64 architecture.

**Migration**: existing snapshot vmstates produced under 4.14 still restore on 6.1 — they just don't gain the new entropy device until they're re-baked. Run `forkd from-image <image> --tag <tag>` for each snapshot you care about to pick up CRNG init + `/dev/hwrng` + VMGENID.

`forkd doctor`'s "kernel image" check now suggests running `scripts/install-guest-kernel.sh` when no vmlinux is found.

## v0.5.0 — 2026-06-05

### Diff-snapshot chains

Snapshots can now record a `parent_tag` + content-hash edge to an
earlier snapshot, letting agents stack incremental layers on a
warmed base (`base → +numpy → +pandas → +sklearn`) instead of
producing a flat 1.5-GiB-each snapshot per dependency set. The
daemon walks the chain at spawn time and assembles the memory image
in one pass; correctness verified across depths 1-3 by the Phase 5
bench ([`bench/chain-spawn/RESULTS-v0.5.md`](./bench/chain-spawn/RESULTS-v0.5.md))
with 90/90 (100%) probe passes. Tracking issue
[#216](https://github.com/deeplethe/forkd/issues/216).

- **Phase 1** (PR #213) — `forkd_vmm::chain` library: `resolve_chain`,
  `verify_parent_hashes`, `assemble_chain_memory` with a reflink-
  preferred `cp(base) + apply_diff` walk. 7 unit tests cover cycle
  detection, missing-parent actionable errors, and hash mismatch.
- **Phase 2a** (PR #214) — chain-aware `POST /v1/sandboxes`: when
  the target tag has `parent_tag` set, the controller transparently
  walks the chain, verifies each link's `parent_content_hash`,
  assembles memory into a per-spawn temp file, and FC-restores from
  that. Pre-existing flat-snapshot spawns unchanged.
- **Phase 2b** (PR #215) — `forkd snapshot-diff --from --tag --exec`
  CLI verb. Spawns a one-shot sandbox from the base, runs the exec
  in-guest, then BRANCHes with `parent_tag` set so the resulting
  snapshot records the chain edge. The daemon's BRANCH endpoint
  accepts `parent_tag` on the request body (validated against the
  source's snapshot tag).
- **Phase 5** (PR #217) — chain-spawn bench + a Phase 2a bug fix.
  Headline: on a 512 MiB base (ext4), a depth-3 chain spawn took
  p50 = 1668 ms vs 746 ms for the flat-equivalent. The ~460 ms
  per-link tax tracks SHA-256 of the base — the mmap-once
  incremental verify optimization is queued for v0.6. Same PR also
  fixed a real bug where the chain-assembled `memory.bin` was
  unlinked by `restore_many_with`'s startup sweep before FC could
  load it (caught by running the bench, never by unit tests). New
  regression test `work_dir_sweep_preserves_chainstage_subdirectory`.
- **Phase 4** (PR #219) — chain-aware management verbs:
  - `forkd snapshot-info <tag>` (`GET /v1/snapshots/:tag/info`) —
    chain depth, ancestors, dependents, on-disk sizes.
  - `forkd rmi <tag> [--cascade | --force]` — pre-Phase-4 `rmi`
    silently orphaned chained children; now refuses with HTTP 409 +
    actionable message until the caller picks an escape hatch.
  - `forkd snapshot-compact --from <tag> --to <new>`
    (`POST /v1/snapshots/:tag/compact`) — materialize a chain into a
    new flat snapshot with no `parent_tag`. Stages via
    `.compact-staging-<to>/` + `rename(2)` so a mid-compact crash
    leaves nothing half-written.
- **Phase 3** (PR #220) — Hub pack/unpack bumped to **manifest v2**.
  When the head has `parent_tag.is_some()`, `forkd pack` walks the
  chain on disk and bundles every ancestor under per-link tar
  prefixes; `unpack` materializes one snapshot directory per link.
  Depth-0 (base) snapshots keep emitting v1 packs so older clients
  keep working. Unsafe `chain[].tag` entries (path traversal) are
  rejected at manifest-parse time before any file body is extracted.
- **Phase 6** — docs (this changelog entry, README v0.5 section,
  `docs/API.md` `SnapshotInfoDetail` + new endpoints, `docs/HUB.md`
  pack v2 spec, [`DESIGN-v0.5-diff-snapshot-chains.md`](./DESIGN-v0.5-diff-snapshot-chains.md)).

### v0.4 live-fork: user-facing surface complete

The v0.4 live-fork path — BRANCH that pauses the source for sub-50 ms
instead of the 150-300 ms a Diff BRANCH costs — is now reachable from
every supported entry point. The underlying mechanism (UFFD_WP to
capture dirty pages out-of-band, memfd-backed RAM so Firecracker and
the controller share the same page cache) was prototyped in Phase 6
(PRs #194-#202). This release wires it through the public surface:

- **REST** (PR #204) — `POST /v1/sandboxes/:id/branch` accepts a
  canonical `mode: "full" | "diff" | "live"` field. The legacy `diff:
  true` boolean still works; setting both yields HTTP 400. New
  `wait: false` toggles fire-and-forget (return after the source
  resumes, ~10 ms; snapshot reaches `status: "ready"` asynchronously).
  `POST /v1/sandboxes` accepts `live_fork: true` so the spawned
  sandbox boots with memfd-backed RAM — a prerequisite for later
  `mode: "live"` BRANCHes from it.
- **CLI** (PR #205) — `forkd snapshot --from-sandbox <id> --live`
  and `--live --no-wait`. Clap enforces `--diff` / `--live` mutex
  and `--no-wait` requires `--live`. **Gap**: `forkd fork
  --live-fork` (spawn-time opt-in) isn't surfaced on the CLI yet —
  use the SDK or `POST /v1/sandboxes` directly to spawn a live-fork
  parent. Tracking as a follow-up.
- **SDKs** (PR #206) — Python (`Controller.branch_sandbox(mode=,
  wait=)`, `Controller.spawn_sandboxes(live_fork=)`), TypeScript
  (`branchSandbox({ mode, wait })`, `spawnSandboxes({ liveFork })`,
  `BranchMode` type re-exported), MCP (`branch_sandbox` /
  `spawn_sandboxes` tools). All backwards-compatible: existing
  `diff=True` callers keep working and still drive v0.3.x daemons
  unchanged.
- **Doctor** (PR #207) — two new checks (`uffd_wp (v0.4 live BRANCH)`
  and `memfd_create (v0.4 live BRANCH)`) probe the kernel
  prerequisites. WARN-not-FAIL: v0.3 Diff/Full still work without
  these. Smart hints — EPERM maps to `sysctl
  vm.unprivileged_userfaultfd=1`, ENOSYS to "kernel < 5.7, Diff
  still works".

**Prereqs for `mode: "live"`**: Linux ≥ 5.7 (UFFD_WP),
`unprivileged_userfaultfd=1` or `CAP_SYS_PTRACE`, the vendored
Firecracker fork from
[deeplethe/firecracker:forkd-v0.4-mem-backend-shared-v1.12](https://github.com/deeplethe/firecracker/tree/forkd-v0.4-mem-backend-shared-v1.12)
— upstream FC doesn't yet ship `mem_backend.shared = true`. See
[`docs/VENDORED-FIRECRACKER.md`](./docs/VENDORED-FIRECRACKER.md).

**Bench numbers** ([`bench/live-fork-pause-window/RESULTS-v0.4.md`](./bench/live-fork-pause-window/RESULTS-v0.4.md))
on a clean Hub-pulled `python-numpy` source (1.5 GiB, Intel i7-12700,
ext4 on HDD):

| mode       | pause p50 | pause p90 | RT p50    |
|------------|----------:|----------:|----------:|
| live-sync  |     56 ms |     64 ms | 13 730 ms |
| live-async |     54 ms |    241 ms | **69 ms** |
| diff       |    202 ms |    418 ms | 13 461 ms |
| full       |  13 550 ms |  14 268 ms | 13 559 ms |

Headline: **3.6× faster pause** vs v0.3 Diff at p50, and the gap
widens on slower storage because Live's pause is disk-independent.
`wait=false` gives callers a ~70 ms HTTP return (vs 13.7 s for sync),
**~200× RT improvement** for fire-and-forget BRANCH.

### Security — bearer-token comparison was a length oracle (closes #162)

`crates/forkd-controller/src/auth.rs::constant_time_eq` was advertised
as constant-time but leaked the presented token's length via two
distinct paths:

1. The length-mismatch branch's "fake work" loop used
   `x.wrapping_mul(0)` — the compiler is allowed to (and LLVM does)
   delete the loop as dead code, so response time was monotonic in
   the longer slice's length.
2. Even if the loop had been preserved, taking different branches
   for length-equal vs length-mismatch was itself a timing oracle.

For a deployment with a fixed-length token (e.g. 32-byte hex from
`forkd doctor`), the length oracle collapses security to a single
attempted length, after which the equal-length branch is real
constant-time.

**Fix.** Replaced with a `subtle::ConstantTimeEq` call against a
zero-padded copy of the presented token. Same code path regardless
of input length; length difference is folded into the result via a
non-short-circuiting bitwise AND. `subtle` was already an indirect
dep via the rustls / aws-lc-rs subtree, so the patch adds no new
crate to the lockfile.

Thanks to @m-dmupba for the report and the precise repro.

## 0.3.4 — 2026-05-23

### Multi-BRANCH pause anomaly fixed (closes #146)

The "BRANCH 3-5 pause_ms jumps to 1.3-1.5 s on the same source" anomaly
documented in v0.3 results is gone. Root cause: ext4's delayed
allocation + multi-block allocator + writeback throttle + block-bitmap
checksumming compounded per BRANCH; each BRANCH's 500 MiB+ memory.bin
write triggered increasing ext4 metadata work.

**Fix**: `posix_fallocate` the destination memory.bin to its full size
right after `mkdir snap_dir`, before the diff-mode background `cp` or
Firecracker's `/snapshot/create` write. ext4 reserves the extents
up-front; subsequent writes don't run `ext4_mb_new_blocks` or update
block bitmaps in-band.

**Measured impact** (ext4 SSD, `coding-agent-fork-prewarm-v1` source,
10 consecutive diff BRANCHes):

| BRANCH | 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 | 9 | 10 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| before | 350 | 250 | 1300 | 1400 | 1500 | 2700 | 1500 | 1800 | 2700 | 1500 |
| after | 585 | 286 | 344 | 161 | 369 | 153 | 189 | 162 | 324 | 174 |

- BRANCH 6: **17.6× faster** (2700 ms → 153 ms)
- Median across BRANCH 3-10: ~1700 ms → ~200 ms (**~8.5×**)
- After-curve matches a tmpfs control to within noise

Best-effort: on filesystems that don't support `posix_fallocate`
(tmpfs, NFS, FAT, etc.) the call returns ENOSYS, a WARN is logged,
and the BRANCH continues with the previous behavior.

Five rounds of probing produced the diagnosis. Full investigation:
[`bench/pause-window/PROBE-multi-branch-anomaly.md`](./bench/pause-window/PROBE-multi-branch-anomaly.md).

### Other

- `forkd doctor` extended to 14 checks (PR #142 from v0.3.3 — listed
  there but worth re-mentioning since it shipped late in the cycle):
  hw-virt, FC version, docker, snapshot-dir space added.
- CI now auto-dispatches `publish-pypi.yml` from `release.yml`
  via explicit `workflow_dispatch` (PR #144). v0.3.1-v0.3.3 all
  required manual `gh workflow run`; v0.3.4 is the first that
  should auto-publish.
- `forkd snapshot --from-sandbox`, `forkd ls`, `forkd kill`,
  `forkd rmi`, `forkd from-image`, `forkd bench`, `forkd doctor`
  all surface a `branch_count` warning when the source has been
  BRANCHed ≥3 times (less relevant now that the anomaly is fixed,
  but kept for diagnostic clarity).

## 0.3.3 — 2026-05-21

### Six new CLI commands

The `forkd` binary gained a developer-experience cluster:

- **`forkd doctor`** — 10 host-readiness checks (KVM, tap, netns,
  firecracker binary, kernel image, daemon, ...) with PASS / WARN /
  FAIL / SKIP per check and a one-line fix hint for each failure.
  Safe to run unprivileged; skips root-only checks with a note. Use
  this first after a fresh `scripts/setup-host.sh`.
- **`forkd bench`** — a representative spawn → exec → branch(diff=true)
  → fanout → cleanup cycle against the live daemon. Screenshot-friendly
  per-step timing. Answers "is forkd actually fast on this box?" in
  one command.
- **`forkd from-image <docker-image> --tag <tag>`** — Docker pull →
  ext4 → boot + warmup → pause → register tag, in one verb. boxlite
  parity for "`docker pull X` and you're done".
- **`forkd ls`** — list live sandboxes the daemon knows about. Table
  output (id / snapshot / pid / netns / guest_addr).
- **`forkd kill <id>...` / `--all` / `--tag <tag>`** — terminate
  sandboxes via DELETE /v1/sandboxes/:id without hand-writing curl.
- **`forkd rmi <tag>...`** — delete snapshot tags (docker-style).
  Tries daemon DELETE first; falls back to direct disk removal when
  the daemon is unreachable or didn't know the tag.

`forkd images` output also got a table refresh: new MEMORY and
CREATED columns (relative age), most-recent-first sort, snapshot
count + total bytes footer.

### `forkd snapshot --from-sandbox --diff`

The CLI now exposes the v0.3 Diff BRANCH path:

```bash
forkd snapshot --from-sandbox sb-abc-0000 --diff --tag base-plus-pip
# ~200 ms pause, vs multi-second Full mode
```

Closes the last gap from #28 — REST and both SDKs already exposed
`diff`; the CLI was the only one missing.

### Five framework integration recipes

Host-side Python scripts (no rootfs build required) showing how to
plug forkd into:

- **`recipes/mcp-agent/`** — Claude Desktop / Cursor / Cline via MCP
- **`recipes/crewai-fanout/`** — N CrewAI agents on N microVMs
- **`recipes/autogen-branch/`** — AutoGen `CodeExecutor` + mid-conversation BRANCH
- **`recipes/openai-swarm/`** — Swarm/Agents handoff = BRANCH
- **`recipes/speculative-agent/`** — **the headline demo**: BRANCH +
  N strategies + judge picks best. Tweet-friendly artifact (2595×
  faster than slowest strategy in the included example).

Each ships with a `--dry-run` mode that exercises the forkd plumbing
without an LLM key.

### Pause-window anomaly probe (#118 thread-level)

Follow-up to v0.3.1's "BRANCH 3-5 anomaly" finding. Two new probe
scripts and a refined attribution: FC is off-CPU ~94 % of the slow
BRANCH window; the dominant contributors are userspace futex
contention (17/250 in-kernel-sleep samples), ext4 journal IO (~2 %),
and un-symbolized user-space CPU on the snapshot worker thread (FC
static-pie release has no frame pointers; needs DWARF or a debug
rebuild to drill further). See
[`bench/pause-window/PROBE-multi-branch-anomaly.md`](./bench/pause-window/PROBE-multi-branch-anomaly.md).
Direct consequence: original #118 Phase 2/3 scope needs revision.

### Other

- Quick start in both `README.md` and `README-zh.md` rewritten around
  `forkd doctor` + `forkd from-image` + `forkd bench` — the modern
  user path that didn't exist before this release.
- `@deeplethe/forkd` 0.3.1 published to npm (first npm release for
  the TS SDK).

## 0.3.2 — 2026-05-20

Python SDK only. Closes the surface-parity gap between the REST API,
the TypeScript SDK, and the Python SDK:

- `Controller.spawn_sandboxes(prewarm=...)` — opt into the v0.2
  prewarm path that amortizes first-BRANCH cold-cache cost.
- `Controller.branch_sandbox(diff=..., measure_diff=...)` — opt into
  v0.3 Diff BRANCH (and the measurement-only Diff sidecar) from
  Python. REST and TS SDK already had these.

No Rust changes; the workspace stayed at 0.3.1.

## 0.3.1 — 2026-05-19

### Phase 1d: multi-BRANCH diff via the previous-output chain

Lifts the v0.3.0 first-BRANCH-only restriction. The daemon now tracks
`SandboxInfo.last_branch_memory_path` — set to whichever BRANCH most
recently completed (Full or Diff). On the next `diff: true` request:

- Chain head set AND file exists → use it as the cp source. By
  construction it's source's complete state at that BRANCH's pause
  time, which is exactly the base the next diff needs.
- Chain head set BUT file missing (user `DELETE`d an intermediate
  BRANCH) → fall back to `source_tag/memory.bin` with a logged
  warning. Lossy but doesn't crash.
- Chain head unset (first BRANCH on sandbox) → use `source_tag/memory.bin`
  as before.

Zero extra storage (each BRANCH's output is already on disk; we just
point at it). No background tasks. No separate shadow file.

The `has_branched: bool` flag stays in `SandboxInfo` as a diagnostic;
the daemon no longer 400s on it. The previously-shipped 400 error
message for second-and-later diff BRANCHes is gone.

Measurement: 3 trials × 5 consecutive `diff: true` BRANCHes on a
mem-2048 SSD source. All 15 succeed. Diff sizes stay <1.2 MB per
BRANCH (bitmap-clear semantics confirmed). Aggregate source downtime
across the 5 BRANCHes is ~4.7 s vs ~70 s if these had been Full —
**14× pause-window reduction over a multi-BRANCH workflow.** Raw
data in `bench/pause-window/multi-branch-sweep.csv`; new sweep script
`bench/pause-window/sweep-multi-branch.sh`.

Anomaly noted but not blocked-on: pause_ms jumps from ~280 ms at
BRANCH 1-2 to ~1.3-1.5 s at BRANCH 3-5 despite the diff size staying
small. Likely a KVM / firecracker control-plane accumulating cost;
filed for follow-up. Still ~10× better than Full mode at every cell.

## 0.3.0 — 2026-05-19

**Headline: source-pause window for BRANCH drops 6-143× depending on workload.**
Idle 4 GiB SSD source: 29 s → 205 ms = 143×.
Typical agent workload (30-300 MiB dirty footprint on 2 GiB source): 6-15×.
Crossover at ~65 % source RAM dirty. Honest curve and practical
guidance in
[`bench/pause-window/RESULTS-v0.3.md`](./bench/pause-window/RESULTS-v0.3.md).

This release also bundles v0.2.5's prewarm fix (#100) — variance
reduction for BRANCH pause on cold-cache hits, sandbox-creation
trade-off — and the v0.3-cycle scaffolding for the deferred live-fork
plan (#101, kept as honest record + revival starting point).

### v0.3 phase 1: diff snapshots — 4 GiB SSD source pause 29 s → 205 ms (143×)

- **`Vm::snapshot_diff_to`** in `forkd-vmm` — calls Firecracker
  `/snapshot/create` with `snapshot_type: "Diff"`, returns a
  `DiffSnapshot` carrying both logical and physical sizes (the latter
  = on-disk allocated bytes = the BRANCH's dirty footprint).
- **`apply_diff(diff_path, base_path)`** helper — `SEEK_DATA`/
  `SEEK_HOLE` walk the diff's allocated extents, 1 MiB chunks copied
  onto the same offsets of the base file. Returns bytes copied.
  Linux-only; non-Linux builds bail rather than silently degrading.
- **`ForkOpts.enable_diff_snapshots: bool`** — required on
  `/snapshot/load` for the resulting VM to admit Diff snapshot/create
  calls. Default false (v0.2 callers preserve identical behavior);
  daemon's `create_sandbox` flips it to true for all daemon-spawned
  sources.
- **`POST /v1/sandboxes/:id/branch` gains `"diff": bool`.** When true,
  the daemon parallelizes the source-tag memory.bin copy with the
  source running, takes a Diff snapshot during a ~200 ms pause,
  resumes the source, joins the copy, and merges the diff onto the
  pre-copied output. The user-visible pause is just the Diff window —
  source TCP connections, kvmclock, and timers see a ~200 ms gap
  instead of seconds. Total BRANCH API latency is unchanged on SSD
  (still bandwidth-bound on the cp); only source DOWNTIME shrinks.
- **`SandboxInfo.has_branched: bool`** + `Registry::mark_branched()`
  gate that rejects second-and-later `"diff": true` BRANCHes with a
  clear 400. Firecracker clears the dirty bitmap on every
  snapshot/create, so a second Diff would silently miss pages dirtied
  before BRANCH 1. Multi-BRANCH diff support (per-sandbox shadow
  file) is deferred to v0.3.1+; forkd's canonical "spawn → BRANCH
  once → fan out N → discard source" workflow only ever takes one
  BRANCH per sandbox, so the restriction covers ~80% of use cases.
- **`SnapshotInfo`** gains `diff_ms`, `diff_physical_bytes`,
  `diff_logical_bytes` — populated when `diff: true` or
  `measure_diff: true` was set on the BRANCH request.
- **Measurement**:
  [`bench/pause-window/RESULTS-v0.3.md`](./bench/pause-window/RESULTS-v0.3.md)
  with the full A/B (5 memory sizes × 3 trials × 2 modes × 2 backends
  = 60 trials). Phase 1a numbers (sidecar Diff inside the existing
  Full pause) match phase 1b numbers (real `diff: true`) within
  measurement noise — architecture validated. Honest caveats: idle-
  source best case; 256 MiB on tmpfs is a wash (control-plane floor
  exceeds memcpy); first-BRANCH-only restriction.
- **Sweep scripts**: `sweep-diff.sh` (phase 1a sidecar timing) and
  `sweep-diff-real.sh` (phase 1b real A/B). Raw data CSVs checked in.

### v0.3 scaffolding (deferred — kept as honest record)

> **Deferred to v0.4+.** Live-fork via memfd + uffd_wp is tracked in
> [issue #101](https://github.com/deeplethe/forkd/issues/101). The
> architecture has an open question on source-divergence sync that we
> haven't sketched concretely enough to commit to weeks of Firecracker
> maintenance for. v0.3 is now pursuing cheaper pause-window wins —
> diff snapshots, NVMe + io_uring, pre-emptive background snapshot —
> see [`docs/ROADMAP.md`](./docs/ROADMAP.md). The scaffolding below
> stays in the repo because it's reusable when/if the project picks
> the live-fork work back up.

- **`MemoryBackend::Userfault` enum variant** in `forkd-vmm`,
  reserved for the (now-deferred) live-branching design. Setting it
  today errors out of `restore_many_with` with a pointer to
  [`docs/design/userfaultfd.md`](./docs/design/userfaultfd.md); no
  caller can accidentally rely on a behavior we haven't built.
- **`forkd-uffd` crate, phase 1.** New workspace member containing the
  library half of the userfaultfd page-fault handler. Implements
  Firecracker's UDS handshake: `recvmsg` with `SCM_RIGHTS` to receive
  the uffd file descriptor + a JSON-encoded `Vec<GuestRegionUffdMapping>`
  describing the host VAs of guest memory regions. Wire-compatible with
  Firecracker v1.10.1's `src/firecracker/examples/uffd/uffd_utils.rs`.
  Ships a `forkd-uffd-handler` binary that accepts the handshake and
  exits — no `UFFDIO_COPY` event loop. Round-trip handshake test
  paired over `socketpair(2)` so CI exercises the parser without
  needing a real Firecracker. Reusable as-is if the live-fork plan
  revives; orthogonal value as a reference implementation of the
  Firecracker uffd protocol.
- **`firecracker-patch/` directory — REMOVED.** Originally drafted
  a ~100 LOC patch for a `MemoryBackend::Memfd` Firecracker extension
  (forking upstream at v1.10.1). After v0.3 phase 1 shipped 143× on
  vanilla Firecracker, we evaluated whether to actually take the fork
  path and decided not to: the memfd value-add doesn't add sharing
  capability we don't already have via `mmap MAP_PRIVATE`, and the
  fork-maintenance cost (own CI, rebase on every upstream tag, track
  CVEs, weakened trust story) isn't justified for the remaining
  pause-window headroom. Reasoning in
  [`docs/design/userfaultfd.md`](./docs/design/userfaultfd.md) §
  "Why we won't fork Firecracker"; revival criteria in
  [issue #101](https://github.com/deeplethe/forkd/issues/101).

### Features

- **Sandbox prewarm: amortize the cold-cache penalty at create time.**
  New `"prewarm": true` field on `POST /v1/sandboxes`. When set, the
  daemon performs a throwaway snapshot to scratch storage
  (configurable, default `/dev/shm/forkd-prewarm`) immediately after
  each child is restored, faulting in all guest pages and populating
  KVM EPT. After prewarm, the first user-visible BRANCH runs at
  steady-state speed instead of paying the measured 2-9x cold-cache
  penalty (see
  [`bench/pause-window/RESULTS-v0.2.md`](./bench/pause-window/RESULTS-v0.2.md)).
  The cold cost is **moved**, not eliminated: sandbox creation pays
  one cold-pause-window worth of latency in exchange for a consistent
  BRANCH latency from the first call. Useful when BRANCH is the
  request handler with an SLO; not useful for create-then-one-BRANCH
  end-to-end latency. Off by default — opt in via the request body.
  Implemented in `forkd-vmm` as `Vm::prewarm()` +
  `ForkOpts::prewarm_scratch_dir`; daemon adds `--prewarm-scratch-dir`
  flag / `FORKD_PREWARM_SCRATCH_DIR` env var.
- **forkd Hub MVP**. `forkd pull <owner>/<name>` resolves through a
  registry.json published in this repo and downloads
  `.forkd-snapshot.tar.zst` packs from GitHub Releases. sha256-
  verified, optional `@<version>` pinning, free public hosting,
  no per-package config. Full spec + publish workflow:
  [`docs/HUB.md`](./docs/HUB.md). First published pack:
  `deeplethe/langgraph-react` (14.5 MiB after 35× zstd
  compression).
- **CLI `forkd pull` rewrite** to use the new registry indirection
  with sha256 integrity check, replacing the previous string-build
  approach against `forkd-hub.deeplethe.com` (which we never set
  up the DNS for).

### Demos / recipes

- **`recipes/langgraph-react/`** — branch-and-fan-out demo of a
  real LangGraph-style ReAct agent. Source agent runs 2 ReAct
  steps with tool calls, BRANCH pauses it (~4 s on SATA SSD,
  ~160 ms on tmpfs), 3 grandchildren
  spawn with different steering hints, each produces a different
  itinerary inheriting the same prior cognitive state. Full writeup
  + asciinema cast embedded in README + first real-run artifacts
  at [`recipes/langgraph-react/results-2026-05-18/`](./recipes/langgraph-react/results-2026-05-18/).
- **`recipes/coding-agent-fork/`** — the "why not parallel prompt?"
  rebuttal. 50 MiB binary blob travels byte-identically across 4
  sandboxes through a single BRANCH; three grandchildren each
  apply different fix strategies (sed / rewrite / skip-tests) and
  produce visibly different outcomes. Artifacts at
  [`recipes/coding-agent-fork/results-2026-05-19/`](./recipes/coding-agent-fork/results-2026-05-19/).
- **`recipes/cube-langgraph/`** (stub) — design sketch for
  CubeSandbox + forkd side-by-side deployment. Pairs with
  [`docs/INTEGRATION-CUBESANDBOX.md`](./docs/INTEGRATION-CUBESANDBOX.md)
  which compares the two projects honestly and proposes 3 concrete
  integration patterns.

### Infrastructure

- **tmpfs `/tmp` mount in `forkd-init.sh`**. Per-VM 256 MiB tmpfs
  prevents the shared-rootfs corruption that hit the langgraph-react
  demo when 3 grandchildren wrote concurrently to the same on-disk
  inode. Affects every recipe built after this commit; no API
  change. **Always put writable demo state under `/tmp`.**

### Benchmarks

- **`bench/pause-window/RESULTS-v0.2.md`** — first-cut measurement
  shows pause is storage-bound: **163 ms ± 7 ms on tmpfs**
  (4 trials), **4.26 s ± 0.41 s on SATA SSD** (5 trials) for the
  same 513 MiB source. Same forkd code; only `--snapshot-root`
  differs. 5/5 connection survival, 0 in-flight loss across SSD
  trials. Surprising mechanism:
  in-guest agents are pause-blind because kvmclock's monotonic
  catch-up on resume races recv data delivery; the recv returns
  data before its timeout timer can fire. The userfaultfd bet's
  value sharpens to "external observers see the pause; in-guest
  agents barely notice".

## 0.1.4 — 2026-05-17

### Security

- **`create_sandbox` snapshot_tag validation gap** (MEDIUM-HIGH,
  post-auth, fixed in PR #54). `POST /v1/sandboxes` accepted
  `req.snapshot_tag` from the request body and joined it directly
  into `snapshot_root` without calling `is_safe_tag` — unlike sister
  handlers `delete_snapshot` and `branch_sandbox` which both
  validated. The unvalidated tag also persisted into
  `SandboxInfo.snapshot_tag` and later flowed into
  `read_snapshot_volumes` during BRANCH, which `serde_json::from_str`'d
  the file at `<snapshot_root>/<tag>/snapshot.json` — letting an
  authenticated attacker control volume mounts of grandchild VMs.
  Full advisory:
  [docs/SECURITY.md → 2026-05-17](./docs/SECURITY.md#past-advisories).
- **K8s manifest placeholder bearer token**. The shipped
  `packaging/k8s/forkd-controller.yaml` ships `token: REPLACE_ME_*`.
  Users who forget to `sed` it before `kubectl apply` would get a
  daemon protected only by a publicly-known token. Fixed: daemon
  refuses to start if the token begins with `REPLACE_ME` /
  `CHANGE_ME` or is shorter than 16 bytes.
- **`boot_wait_secs` cap**. `POST /v1/snapshots` previously accepted
  any `u64`. Clamped to 60 s.

### Reliability

- **BRANCH concurrency caps** (PR #56). Two `POST /branch` calls on
  the same target tag now serialise via a per-tag in-flight set
  (second gets 409) — previously both could pass the
  `vmstate.exists()` TOCTOU and clobber each other. The daemon also
  admits at most `DEFAULT_BRANCH_CONCURRENCY` (4) BRANCHes
  simultaneously; excess gets 503. Both bounds use an RAII
  `BranchSlot` guard so every error path releases cleanly.

### Observability

- **`pause_ms` on BRANCH responses** (PR #58). `SnapshotInfo` now
  carries an optional `pause_ms` populated by `branch_sandbox` with
  the measured `pause() → resume()` envelope on the source VM.
  Also emitted as a structured `tracing::info!` event. Powers the
  new `bench/pause-window/` harness.

### Benchmarks

- **Pause-window harness + first-cut results**. New
  `bench/pause-window/` directory with a synthetic ping/pong agent,
  host-side echo server, orchestrator, and pure-function analyzer
  (14 unit tests, covered by a new `bench-python` CI job). 5
  trials on real hardware (513 MiB source): mean pause
  **4.26 s ± 0.41 s**, **0 in-flight loss, 5/5 connection survival**.
  Surprising mechanism (in-guest agents are nearly pause-blind via
  kvmclock catch-up) documented in
  [`bench/pause-window/RESULTS-v0.2.md`](./bench/pause-window/RESULTS-v0.2.md).

## 0.1.3 — 2026-05-14

### Security

- **Path traversal via `--tag`** (CVE-class, fixed). `forkd snapshot`,
  `forkd unpack`, `forkd pull`, `forkd fork`, `forkd pack`, and
  `forkd push` accepted arbitrary strings for `--tag` and used them
  in `Path::join`, which silently discards the base when the right
  side is absolute. A tag like `/etc/forkd-bad` or `../../etc/x`
  could write Firecracker snapshot files outside the data directory.
  The same risk extended to the `tag` field of `manifest.toml`
  inside a Snapshot Hub pack — a malicious or compromised pack
  could write its files anywhere the running user can write.
  Affects 0.1.0–0.1.2. Fixed by validating tags against
  `[A-Za-z0-9_][A-Za-z0-9._-]{0,63}` at every CLI surface and again
  on the manifest's `tag` field. Full advisory:
  [docs/SECURITY.md → Past advisories](./docs/SECURITY.md#past-advisories).
- `forkd cleanup` would mis-classify live VMs as "safe to delete"
  because `lsof` returns empty stdout (only warnings on stderr) for
  Firecracker UNIX domain API sockets. Under `forkd cleanup --yes`
  this would have torn down the work_dir of an actively running
  VM. Replaced the detection with a `/proc/<pid>/fd/*` readlink
  scan that explicitly checks whether any process holds an open
  handle inside the candidate directory.

### Added

- `forkd push <local-tag> <url>` — HTTP PUT a packed snapshot to
  any URL (presigned PUT from R2/S3/etc. is the intended fit).
- `forkd cleanup` — sweep orphan `/tmp/forkd-{fork,parent,unpack,
  pull}-*` work directories left behind by crashed or killed
  runs. Dry-run by default; `--yes` to actually delete. Refuses
  to touch directories whose `/proc` fd scan shows a live process.
- `scripts/netns-teardown.sh` — reverse `netns-setup.sh`.
  Dry-run by default, removes only `^forkd-child-[0-9]+$` netns.
  Docker bridges, system tap, and `forkd-br0` are untouchable
  without explicit `--include-bridge` / `--include-tap` flags.
- `forkd snapshot --mem-size-mib` — override the parent VM
  memory size (default 512 MiB). Required for memory-hungry
  warmup workloads; browser recipes need ≥ 2048 MiB to avoid
  Chromium OOM during snapshot.
- `forkd snapshot --keep-workdir` / `forkd fork --keep-workdir` —
  preserve `/tmp/forkd-{parent,fork}-<tag>/` after a successful
  run for post-mortem inspection. The default behaviour now
  removes the work_dir on success (failure paths still preserve
  it).
- Pre-flight check on `forkd snapshot` / `forkd fork` refuses to
  start when another forkd run on the same tag is already in
  flight (live process holding sockets in the same work_dir), and
  cleans stale work_dirs from earlier crashes before proceeding.
- Snapshot Hub: pack-and-go via `forkd pack` / `forkd unpack` /
  `forkd pull`. Manifest records per-file sha256, format version,
  and a reserved `parent_tag` slot for the M2.1 diff-snapshot
  chain work.
- `recipes/playwright-browser/` — fork a warmed headless Chromium
  parent. Each child VM inherits a fully-initialised browser via
  mmap CoW; per-call `sb.eval("await page.title()")` returns in
  ~10–80 ms instead of the ~2 s required for a cold Chromium
  spawn. Requires `--mem-size-mib 2048`.
- `recipes/jupyter-kernel/` — SciPy-warm parent for code-
  interpreter workloads.
- `forkd-agent.py` recipe-level eval bridge. When the rootfs
  contains `/etc/forkd-recipe.env` declaring `FORKD_WARMUP_CMD`
  and `FORKD_AGENT_LANG=node`, the agent multiplexes
  `sb.eval(<js>)` calls to a warmup subprocess over a
  line-JSON protocol. `Sandbox.eval()` deserialises the reply
  back into a native Python object.
- `ROADMAP.md` documenting M1 / M2 / M3 milestones.
- README Snapshot Hub section, Chinese translation
  (`README-zh.md`), PyPI version badge.

### Changed

- `forkd eval` now prints the `result_json` field returned by
  Node-recipe replies; previously this surface was silently
  dropped on the CLI side. Python recipes' `result` (repr-string)
  path unchanged for backwards compatibility.
- Warmup process inside `playwright-browser` emits sentinel
  strings (`__js_Infinity__`, `__js_-Infinity__`, `__js_NaN__`)
  for non-finite JavaScript numbers, which `JSON.stringify`
  otherwise silently converts to `null`. Takes effect for any
  recipe rebuilt against 0.1.3.
- Error messages on `forkd unpack`, `forkd pull`, integrity
  failures, manifest parse errors, and HTTP failures now show
  the underlying `Caused by:` chain with operator-actionable
  hints (DNS failure, expired presigned URL, corrupted pack,
  etc.).

### Internal

- `crates/forkd-cli`: new `hub` module for pack format + push/pull.
- `rootfs-init/tests/` — host-runnable smoke tests for the
  recipe eval bridge (`fake-warmup.py`, `smoke-test.sh`,
  `smoke-sdk.py`).
- CI: branch-protected `main` with `rust` job (fmt + clippy +
  test) required; PyPI Trusted Publisher (OIDC) workflow.

## 0.1.2 — 2026-05-12

- Python SDK published to PyPI (`pip install forkd`).
- CubeSandbox row in the README benchmark table now leads with
  the bare-metal-host context after a `systemd-detect-virt: none`
  proof, to address the "nested virtualisation might be skewing
  the numbers" concern raised upstream.

## 0.1.1 — 2026-05-11

- README "Where forkd fits" rewritten with 5 concrete use cases.
- Initial GitHub Release pipeline.

## 0.1.0 — 2026-05-10

- Initial public release. Fork-on-write microVM primitive,
  controller daemon, REST API, Python SDK, six recipes, and
  the N=100 spawn benchmark.
