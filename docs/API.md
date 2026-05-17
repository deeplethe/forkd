# forkd HTTP API (v1)

The forkd controller exposes a JSON/HTTP API on `127.0.0.1:8889` by
default. Pass `--tls-cert`/`--tls-key` to serve HTTPS instead of
plain HTTP. All routes except `/healthz` require a bearer token when
the daemon is started with `--token-file`.

```http
Authorization: Bearer <contents-of-token-file>
```

API versioning: every breaking change moves to a new `/vN` prefix. The
controller will support the previous major in parallel for one minor
release after the new one ships.

---

## Status and discovery

### GET /healthz

Liveness probe. Always returns 200. Bypasses authentication so that
load balancers can probe the daemon without a credential.

```json
{ "ok": true }
```

### GET /version

```json
{ "version": "0.1.0", "api": "v1" }
```

### GET /metrics

Prometheus text exposition format. Stable metric names:

- `forkd_snapshots_total` (gauge) — registered snapshots
- `forkd_sandboxes_active` (gauge) — currently-alive child VMs
- `forkd_build_info{version="X.Y.Z"}` — always 1, label carries the build version

---

## Snapshots

### POST /v1/snapshots

Build a snapshot of a freshly booted parent VM and register it under
`<tag>`. Blocks for `boot_wait_secs` while userspace warms up inside
the guest.

Request:

```json
{
  "tag": "py",
  "kernel": "/var/lib/forkd/kernels/vmlinux-6.1",
  "rootfs": "/var/lib/forkd/rootfs/python.ext4",
  "rw": true,
  "tap": "forkd-tap0",
  "boot_wait_secs": 10
}
```

Response (201 Created):

```json
{
  "tag": "py",
  "dir": "/var/lib/forkd/snapshots/py",
  "created_at_unix": 1717000000
}
```

Errors:

- `400 Bad Request` — invalid `tag`, missing `kernel`/`rootfs`, snapshot already exists
- `500 Internal Server Error` — Firecracker boot/snapshot failure (see logs)

### GET /v1/snapshots

List registered snapshots: `[SnapshotInfo, ...]`.

### DELETE /v1/snapshots/:tag

Remove the registry entry and delete the on-disk snapshot files.
Returns `204 No Content`. `404` if no such tag is registered and no
on-disk files exist.

---

## Sandboxes

### POST /v1/sandboxes

Fork N children from a registered snapshot tag.

Request:

```json
{
  "snapshot_tag": "py",
  "n": 10,
  "per_child_netns": true,
  "memory_limit_mib": 256
}
```

- `n` — 1 ≤ n ≤ 1000
- `per_child_netns` — when true, each child is placed in
  `forkd-child-<i>`; the host must have provisioned those namespaces
  via `scripts/netns-setup.sh N` first.
- `memory_limit_mib` — sets `memory.max` on a per-child cgroup v2
  leaf. Requires cgroup v2 unified hierarchy and write access to
  `/sys/fs/cgroup/forkd/`.

Response (201 Created): `[SandboxInfo, ...]`.

### GET /v1/sandboxes

List active sandboxes.

### GET /v1/sandboxes/:id

One sandbox's metadata.

### DELETE /v1/sandboxes/:id

Terminate. Kills the Firecracker process and removes the cgroup leaf.
Returns `204 No Content`.

### POST /v1/sandboxes/:id/ping

Round-trip to the guest agent inside the VM.

```json
{ "pong": true, "numpy_version": "1.26.4", "pid": 1 }
```

### POST /v1/sandboxes/:id/exec

Spawn a subprocess in the sandbox.

Request:

```json
{ "args": ["python3", "-c", "print(2+2)"], "timeout_secs": 30 }
```

Response:

```json
{ "stdout": "4\n", "stderr": "", "exit_code": 0 }
```

### POST /v1/sandboxes/:id/eval

Evaluate a Python expression against the already-warmed interpreter
running as PID 1.

Request: `{ "code": "numpy.zeros(5).sum()" }`

Response: `{ "result": "0.0", "error": null, "exit_code": 0 }`

### POST /v1/sandboxes/:id/branch

Pause a running sandbox, snapshot its memory + vmstate to a new tag,
resume it. The resulting snapshot is independent of the source's
lifecycle — fork from it or delete it regardless of whether the source
sandbox is still alive. Volumes from the source snapshot are inherited
automatically, so grandchildren see the same persistent disks.

Request:

```json
{ "tag": "checkpoint-1" }
```

- `tag` is optional. When unset the daemon generates
  `branch-<source-id>-<unix-ts>`. Must match
  `^[A-Za-z0-9_][A-Za-z0-9._-]{0,63}$`.

Response (201 Created): [`SnapshotInfo`](#snapshotinfo) with
`branched_from` set to the source sandbox id and `pause_ms`
populated with the measured pause window in milliseconds.

Errors:

- `404 Not Found` — source sandbox id not in `live_vms`
- `409 Conflict` — tag already exists on disk; `DELETE` it first
- `409 Conflict` — a BRANCH for this exact tag is already in flight
- `503 Service Unavailable` — daemon at branch concurrency cap (default 4)
- `500 Internal Server Error` — pause / snapshot / resume failure

**Pause-window semantics.** The source sandbox is paused at the vCPU
level (kernel state and TCP sockets stay; application-level keepalives
may time out) for the duration of the snapshot write — typically
0.5–8 s depending on memory image size. Modal's "branch" operation
has the same semantics.

If `resume` fails after a successful snapshot the snapshot file is
intact and returned to the caller; the source sandbox may be left in
an unknown state. The controller logs this as a warning rather than
failing the request, because the user's primary expectation (a valid
new snapshot) has been met.

See [`docs/design/branching.md`](design/branching.md) for the full
rationale, use cases, and follow-up roadmap.

---

## SandboxInfo

```json
{
  "id": "sb-67a1b3-0000",
  "snapshot_tag": "py",
  "netns": "forkd-child-1",
  "guest_addr": "10.42.0.2:8888",
  "created_at_unix": 1717000123,
  "pid": 314159,
  "memory_limit_mib": 256
}
```

## SnapshotInfo

```json
{
  "tag": "py",
  "dir": "/var/lib/forkd/snapshots/py",
  "created_at_unix": 1717000000,
  "branched_from": "sb-67a1b3-0000",
  "pause_ms": 1820
}
```

- `branched_from` is **omitted** when the snapshot was built from
  kernel + rootfs via `POST /v1/snapshots`; it is **present**
  (carrying the source sandbox id) only when the snapshot was
  produced via `POST /v1/sandboxes/:id/branch`. Use this field to
  trace snapshot lineage / audit.
- `pause_ms` is the measured source-VM pause window in milliseconds
  (`pause() → resume()` envelope). Omitted for snapshots not produced
  via BRANCH. This is the daemon's ground truth; the *application*-
  observed pause (TCP stalls, missed pings) can be longer due to OS
  retransmit timers.

## ErrorBody

Every 4xx and 5xx response carries:

```json
{ "error": "human-readable message" }
```
