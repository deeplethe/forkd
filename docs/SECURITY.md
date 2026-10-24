# Security policy

forkd is alpha software. The threat model and current guarantees are
documented below so operators can decide what workload they are
willing to point at it.

## Which boundary you actually get

**This section used to say every child ran in a KVM-backed microVM, so
escaping needed a KVM or Firecracker vulnerability -- "the same boundary
AWS Lambda relies on". That is not what `forkd-serve` runs.** It is written
out here rather than quietly edited, because anyone who read it and sized
their risk by it sized it against the wrong kernel.

v0.6 ships one sandbox backend, gVisor (`runsc`), and the isolation it gives
is weaker than a microVM's in a specific way: the host kernel is shared.

| | `forkd-serve` (v0.6) | `forkd-vmm` (via `forkd cli`) |
|---|---|---|
| Sandbox | gVisor `runsc` container | Firecracker microVM |
| Guest kernel | the gVisor Sentry, in user space | its own Linux kernel |
| Host kernel | **shared with every sandbox** | reached only through KVM |
| Escaping needs | a Sentry bug, or a hole in the host seccomp behind it | a KVM or Firecracker bug |
| Needs `/dev/kvm` | no | yes |

**Why gVisor is the default.** Firecracker needs `/dev/kvm`, and most cloud
instances do not expose nested virtualisation -- none of the three machines
forkd is developed and measured on do. A backend that cannot run on the
machines operators actually have is not a default. The Firecracker backend
remains supported for hosts that do have KVM, and is the stronger choice
there; its four-tier numbers have not been measured, because measuring them
needs such a host (see `DESIGN-v0.6.md` section 13).

So, concretely:

1. **The host kernel, and `runsc` or Firecracker, are part of the TCB.** A
   compromised host can do anything to its sandboxes. forkd does not try to
   protect against a hostile administrator.

2. **Sandboxes are mutually untrusted, with the boundary above.** Each
   sandbox gets its own network namespace, its own cgroup and its own
   writable layer. Under `runsc` they also share one host kernel, so a
   Sentry escape reaches the host and therefore its siblings. Do not treat
   a `runsc` sandbox as equivalent to a VM for hostile code; if that is your
   workload, run the Firecracker backend on a KVM host.

3. **The control plane is a root credential.** The API's bearer token grants
   full control over the sandboxes and snapshots of every node attached to
   that master. Nodes additionally authenticate with client certificates
   (see below), but the API token is not scoped -- treat it as root.

## Default posture

These are `forkd-serve`'s defaults. Several were off in v0.5 and are now
on, so a v0.5 runbook will describe a weaker posture than you have.

| Concern | Default | Notes |
|---|---|---|
| Daemon bind | `127.0.0.1:8080` (loopback only) | `--bind` to expose it; do not do that without the token below |
| Control-plane TLS | **on** | `--insecure-transport` turns it off for a single box on loopback, and logs what that gives up |
| Node authentication | **client certificates, required** | A node's identity is the certificate the master issued it, not the node id in its messages. `POST /v1/node/revoke/:id` stops one |
| Registration | single-use token, 15 min, carrying the CA fingerprint to pin | So a stand-in master fails the handshake before the token is sent to it |
| API authentication | **on**, token generated on first run | `--api-token-file`; `--api-open` disables it and warns. Code inside a sandbox can reach the master over loopback, so an open API lets a sandbox mint node tokens |
| Per-sandbox netns | **always** | Not an option any more. `per_child_netns` is accepted and ignored |
| Sibling and egress traffic | **dropped** (`FORWARD`) | Covers siblings and the internet without relying on `ip_forward` |
| Host services on the veth | **dropped** (`INPUT`) | sshd listens on `0.0.0.0`, so it answers on the veth too; only established replies are let back |
| Cold sandbox ports | **refused** (`FORKD-COLD`, TCP reset) | A caller reaching a frozen, snapshotted or detached sandbox gets 7-12 ms instead of its own timeout |
| Per-sandbox memory cap | none | `memory_limit_mib` per sandbox, written as `memory.max` |
| Audit log | `/var/log/forkd/audit.log`, JSON lines | tail with vector / fluentbit; rotate with logrotate |

## Known gaps

Stated because each is the kind of thing that reads as covered when it is
not:

- **A sandbox's `token` is issued and never checked.** Every sandbox is
  created with a 128-bit token and it is returned to the caller, but nothing
  verifies it, because there is no gateway in front of a sandbox for it to
  gate: a sandbox listens inside its own namespace and the rules above drop
  traffic from siblings and from off-box, so today it is reachable only from
  its own node. The moment a port is exposed, that token has to become
  mandatory on the path that exposes it -- `DESIGN-v0.6.md` section 16.5
  calls this layer two, and it is not built.
- **Sandbox ids are `sb-` plus 12 hex characters**, not 128 bits. Section
  16.5 asks for unguessable ids; this is weaker than that.
- **One API token, no scopes or tenants.** The `owner` column exists in the
  schema and nothing reads it.
- **Multi-node has never been run.** Master and node authenticate each other
  properly now, but they have only ever run on one machine.

## Kubernetes deployment

The shipped `packaging/k8s/forkd-serve.yaml` runs the daemon
with `privileged: true`, `runAsUser: 0`, and a writable
`/sys/fs/cgroup` hostPath mount. This is **necessary** — the node creates
network namespaces and veth pairs, writes cgroup v2 for the tiers and the
memory caps, and runs `runsc`. (It does *not* need `/dev/kvm`: that is the
Firecracker backend's requirement.) It is also **node-level blast-radius**:
a compromised `forkd-serve` pod can escape to the node it runs on.

Operational consequences:

- Treat the `forkd-serve` pod's bearer token like SSH-root on the
  node. Rotate on any access change.
- Pin the pod to a dedicated node pool. Do not co-schedule untrusted
  tenants.
- The daemon refuses to start if the manifest's placeholder bearer
  token (`REPLACE_ME_*` / `CHANGE_ME_*`) is left in place — a forgotten
  `sed` step becomes a noisy fail rather than a silent compromise.
- For multi-tenant deployments, run one master per tenant on dedicated
  nodes rather than sharing one. The API token is not scoped, so sharing a
  master means sharing root over every tenant's sandboxes.

## Concurrency caps

`DEFAULT_BRANCH_CONCURRENCY` and the `boot_wait_secs` cap were
`forkd-controller`'s, and went with it. What v0.6 bounds instead:

- **Fork width.** One request may ask for at most `MAX_FORK_WIDTH` (256)
  children of a tag.
- **Admission.** A node stops accepting new sandboxes below a free-memory
  floor or above a memory-pressure average (`--admit-min-free-mib`,
  `--admit-max-psi`). This exists because without it the master kept
  accepting creates until the machine stopped answering SSH; the measured
  curve is in `master/api.rs`.
- **Checkpoint size** is not bounded. Measured at 50.2 MiB per sandbox, so a
  thousand snapshotted sandboxes is about 50 GB of disk. Admission looks at
  memory and pressure, not disk.

## TLS

There are no certificate flags any more, and nothing to obtain: the master
runs its own CA under `--pki-dir`, creating it on first start, and issues
itself a server certificate and each node a client certificate from it.
rustls 0.23 with the `ring` provider; TLS 1.2 and 1.3, no legacy suites.

Operationally:

- **The control plane's PKI is internal and should stay that way.** It
  authenticates nodes to the master and the master to its nodes; it is not
  a web PKI and a public CA has no role in it. Keep `--pki-dir` backed up
  with the state database: losing the CA key means re-registering every
  node.
- **A node pins the CA on first contact** through the fingerprint carried in
  its registration token (`<fingerprint>.<secret>`). Pass the token the
  master printed, unaltered; splitting it loses the pin and the node will
  warn that it cannot verify the master.
- **Revoke, do not rotate, to remove a node:** `POST /v1/node/revoke/:id`.
  Its parked connection is dropped at once.
- **TLS does not imply API authentication.** The bearer token is separate
  and is on by default; `--api-open` turns it off and says so loudly.
- If you terminate TLS somewhere else, the master must still be the thing
  nodes verify. A proxy holding its own certificate breaks the pin, and
  client certificates do not survive it, so nodes lose their identity and
  the master falls back to believing what they claim.

## What forkd does not do (yet)

- **Multi-node scheduling.** One daemon = one host. No HA, no failover.
- **Egress allow-lists.** Egress is denied outright rather than filtered:
  `FORWARD` drops it. That is safe, and also means a sandbox cannot reach
  the internet at all, so there is no policy to express yet. Giving one
  controlled outbound access is unbuilt.
- **Quotas beyond memory.** `cpu.max`, `io.max` and `pids.max` are not
  wired through the API.
- **Third-party security audit.** Not started. Will be required
  before forkd claims a "production" status badge.

## Reporting a vulnerability

Email `security@deeplethe.com`. Please do not open a public issue for
security reports. We aim to acknowledge within 72 hours and ship a fix
or mitigation within 14 days for confirmed issues.

## Supported versions

Pre-1.0 releases receive fixes only on the latest minor. Fixed
vulnerabilities are recorded in the CHANGELOG under the release that
fixed them, with the affected versions.
