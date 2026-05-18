# Changelog

Notable changes per release. forkd follows [Semantic
Versioning](https://semver.org/spec/v2.0.0.html) once it reaches
1.0; until then, the minor version can break compatibility.

## Unreleased — 0.1.5 (in flight)

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
- **`firecracker-patch/`** — design doc and first-cut `.patch` for
  the `MemoryBackend::Memfd` Firecracker extension. Not compile-tested.
  Deferred along with the rest; the README in that directory now
  carries a prominent "DEFERRED" banner pointing at #101.

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
