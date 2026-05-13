# Changelog

Notable changes per release. forkd follows [Semantic
Versioning](https://semver.org/spec/v2.0.0.html) once it reaches
1.0; until then, the minor version can break compatibility.

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
