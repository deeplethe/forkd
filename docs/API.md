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

### GET /metrics

Prometheus text exposition (`text/plain; version=0.0.4`). **Behind the
bearer token**, unlike `/healthz`: it names every node, its free memory
and the shape of the fleet, which is a map of the deployment. Configure
the scraper with the token.

```yaml
scrape_configs:
  - job_name: forkd
    scheme: https
    authorization: { credentials_file: /etc/forkd/api-token }
    tls_config: { ca_file: /var/lib/forkd/pki/ca.pem }
    static_configs: [{ targets: ["master:8080"] }]
```

Three kinds of series:

| | |
|---|---|
| `forkd_sandbox_creates_total{outcome}`, `forkd_sandbox_deletes_total`, `forkd_demotions_total{to}` | what the control plane was asked to do |
| `forkd_http_request_duration_seconds{method,route}`, `forkd_store_lock_wait_seconds`, `forkd_store_lock_held_seconds`, `forkd_demote_sweep_seconds` | how long it took |
| `forkd_nodes{state}`, `forkd_node_*{node,pool}`, `forkd_sandboxes{tier}`, `forkd_sandboxes_failed`, `forkd_snapshots`, `forkd_snapshot_bytes` | what the fleet looks like now |

Two things to know before reading a graph:

- **`route` is the matched path**, so `/v1/sandboxes/:id` is one series
  rather than one per sandbox id.
- **`/v1/node/poll` is a long poll.** The master parks it for up to
  `--hold-secs` when it has nothing to say, so its duration is the hold,
  not a cost. It has its own series; do not average it in with the rest.

A capacity refusal is a 503, so it lands in
`forkd_http_responses_total{class="server_error"}`.
`forkd_sandbox_creates_total{outcome="refused"}` is what separates "the
fleet is full" from "the master is broken".

### GET /version

```json
{ "version": "0.1.0", "api": "v1" }
```

---

## Nodes

A node is driven one of two ways. Both run the same reconciliation; only
which side opens the connection differs.

| | node runs | how it joins | use it when |
|---|---|---|---|
| **listen** | `serve --node --listen 0.0.0.0:80` | the master is given its address and pairing code | the master can reach the node |
| **pull** | `serve --node --master-url …` | the node redeems a registration token | only the node can reach the master, e.g. behind NAT |

### POST /v1/nodes

Take over a node that is listening. The node is already installed and
running; this is the whole of joining it.

```json
{
  "address": "10.2.4.17:80",
  "pairing_code": "68fed7b4…<64 hex>.<32 hex>",
  "node_id": "f3",
  "pool": "default"
}
```

`address` may omit the port (80 is assumed); write IPv6 as `[addr]:port`.
`node_id` defaults to what the node reports (its `--node-id`, else its
hostname). `pool` is a request, not a guarantee: a node whose host
fingerprint differs from the pool's joins `<pool>#<fp8>` instead, exactly
as a registering node does.

The **pairing code** is `<certificate SHA-256>.<secret>`, written by the node
to `/var/lib/forkd/pki/node/pairing-code` (root only) on first start and kept
across restarts. The master pins the node's TLS certificate by the first
half and presents the second on every request, so neither side can be
impersonated. Because the code does not change, it is a standing
credential for that node -- whoever holds it can drive every sandbox there.
To change it, delete `pairing-secret` beside it and restart the node.

| status | meaning |
|---|---|
| `201` | added; the master is driving it |
| `400` | malformed address or code -- nothing was dialled |
| `409` | a node with this id is connected and polling right now; stop that service first |
| `502` | the node could not be reached, presented a different certificate, or refused the secret -- the body says which |

The response is `{id, pool, address, fingerprint, max_sandboxes}`. The node
shows as connected in `GET /v1/nodes` once the master's first sync lands,
normally within a second.

### POST /v1/node/revoke/:id

Stop trusting a node. A polling node's certificate is refused from then on;
a listening node is no longer contacted. Re-adding a listening node with its
pairing code lifts the revocation.

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

**v0.5 chain safety** — when the tag is the recorded `parent_tag` of
one or more other snapshots, the daemon refuses with `409 Conflict`
unless the caller opts in via a query parameter:

| Query | Effect |
|---|---|
| `?cascade=true` | Recursively delete the tag AND every descendant snapshot. |
| `?force=true`   | Delete the tag, leaving children orphaned (they will fail to restore). |

The two flags are mutually exclusive; passing both returns
`400 Bad Request`. The 409 response body names every blocking
dependent so the caller can decide:

```json
{
  "error": "snapshot `py-numpy` is the parent of 1 chained snapshot(s): [py-pandas]; rerun with `?cascade=true` to delete the whole subtree, or `?force=true` to orphan the children (they will fail to restore)"
}
```

### GET /v1/snapshots/:tag/info

**v0.5.** Return chain + on-disk info for a single snapshot. Useful
before `rmi`-ing a chained tag (see the dependents list) or before
deciding to `compact` a deep chain (see the depth + per-link sizes).

Response body shape:

```json
{
  "tag": "py-pandas",
  "dir": "/var/lib/forkd/snapshots/py-pandas",
  "created_at_unix": 1780556400,
  "memory_logical_bytes": 536870912,
  "memory_physical_bytes": 536875008,
  "vmstate_bytes": 21900,
  "parent_tag": "py-numpy",
  "parent_content_hash": "276c99e9...",
  "chain_depth": 2,
  "ancestors": ["py-base", "py-numpy"],
  "dependents": []
}
```

`chain_depth` counts diff links between the snapshot and its chain
root (0 for a base). `ancestors` is ordered root → direct parent.
`dependents` lists every tag whose `parent_tag` equals this one.

`404 Not Found` when the tag isn't registered and has no on-disk
directory.

### POST /v1/snapshots/:tag/compact

**v0.5.** Walk the chain rooted at `tag`, verify every per-link
parent content hash, assemble the head's memory image, and persist
the result as a new flat (parentless) snapshot under `req.to`. The
new snapshot restores via the original non-chain code path with no
per-link SHA-256 tax.

Request:
```json
{ "to": "py-pandas-flat" }
```

Response: a `SnapshotInfo` for the new flat snapshot. `409 Conflict`
when `req.to` already exists. `400 Bad Request` when `req.to ==
:tag` or either tag is unsafe.

The implementation stages to a sibling `.compact-staging-<to>/`
directory and `rename(2)`s into place, so a mid-compact crash never
leaves a half-written destination snapshot behind.

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
  "memory_limit_mib": 256,
  "live_fork": true
}
```

- `n` — 1 ≤ n ≤ 1000
- `per_child_netns` — when true, each child is placed in
  `forkd-child-<i>`; the host must have provisioned those namespaces
  via `scripts/netns-setup.sh N` first.
- `memory_limit_mib` — sets `memory.max` on a per-child cgroup v2
  leaf. Requires cgroup v2 unified hierarchy and write access to
  `/sys/fs/cgroup/forkd/`.
- `live_fork` (v0.4+, default `false`) — boot the sandbox with a
  memfd-backed RAM region so later `POST .../branch` calls can use
  `mode: "live"`. Requires Linux ≥ 5.7 and the vendored Firecracker
  fork (see `docs/VENDORED-FIRECRACKER.md`). `forkd doctor` probes
  both prerequisites.

Response (201 Created): `[SandboxInfo, ...]`.

Errors:

- `404 Not Found` - snapshot tag not found.
- `409 Conflict` - snapshot tag exists but is not restorable yet, failed,
  or is missing a required artifact such as `vmstate`, `memory.bin`, or
  its recorded `rootfs`; the response body names the failing path.
- `500 Internal Server Error` - Firecracker restore failure.

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
{ "tag": "checkpoint-1", "mode": "live", "wait": false }
```

- `tag` is optional. When unset the daemon generates
  `branch-<source-id>-<unix-ts>-<hex-suffix>`. Must match
  `^[A-Za-z0-9_-]{1,64}$` (1–64 chars, ASCII alphanumeric plus `-`/`_`).
- `mode` (v0.4+) is one of `"full"`, `"diff"`, `"live"`. Defaults to
  `"full"` when unset. `"live"` requires the source sandbox to have
  been spawned with `live_fork: true` and the host to support UFFD_WP
  + memfd_create (`forkd doctor` probes both).
- `diff: true` is the legacy v0.3 equivalent of `mode: "diff"`; kept
  for compatibility. **Mutually exclusive with `mode`** — sending
  both yields `400 Bad Request`.
- `wait` (v0.4+, default `true`) is only meaningful with
  `mode: "live"`. When `false`, the daemon returns
  [`SnapshotInfo`](#snapshotinfo) with `status: "writing"` as soon
  as the source resumes (~10 ms); the background memory copy
  finishes asynchronously and the snapshot's `status` flips to
  `"ready"` (or `"failed"`). Poll `GET /v1/snapshots` to detect
  completion.

Response (201 Created): [`SnapshotInfo`](#snapshotinfo) with
`branched_from` set to the source sandbox id and `pause_ms`
populated with the measured pause window in milliseconds. With
`mode: "live"`, also returns `status` (`"writing"` when
`wait: false`, otherwise `"ready"`).

Errors:

- `400 Bad Request` — both `mode` and `diff` set
- `404 Not Found` — source sandbox id not in `live_vms`
- `409 Conflict` — tag already exists on disk; `DELETE` it first
- `409 Conflict` — a BRANCH for this exact tag is already in flight
- `503 Service Unavailable` — daemon at branch concurrency cap (default 4)
- `500 Internal Server Error` — pause / snapshot / resume failure

**Pause-window semantics by mode.** The source's user-visible pause:

- `mode: "full"` — 0.5–8 s, whole guest RAM written.
- `mode: "diff"` — ~200 ms idle source, sub-second for typical agent
  workloads (v0.3+; see `bench/pause-window/RESULTS-v0.3.md`).
- `mode: "live"` — sub-50 ms; dirty pages captured asynchronously
  via UFFD_WP (v0.4+).

The source is paused at the vCPU level (kernel state and TCP sockets
stay; application-level keepalives may time out for `"full"`).
Modal's "branch" operation has comparable semantics to `mode: "full"`.

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
  "pause_ms": 1820,
  "status": "ready",
  "bootable": true
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
- `status` (v0.4+, optional) — `"writing"` while a live BRANCH's
  background memory copy is in flight (only seen with `mode: "live"`
  + `wait: false`), `"ready"` once the snapshot is consumable,
  `"failed"` if the background copy hit an error. Omitted on
  snapshots from Diff or Full BRANCH (they're synchronous, so the
  daemon only returns once they're `ready`).

- `bootable` (v0.5.3+, boolean) - derived from the snapshot artifacts
  visible to the controller. `false` means `POST /v1/sandboxes` will
  reject the tag before restore; `warning` names the missing or corrupt
  artifact when the daemon can determine it.

## SnapshotInfoDetail (v0.5)

Returned by `GET /v1/snapshots/:tag/info`. Adds chain + on-disk
fields to the registry's plain `SnapshotInfo` shape:

| Field | Type | Notes |
|---|---|---|
| `tag` | string | The queried tag. |
| `dir` | string | Absolute path to the snapshot directory. |
| `created_at_unix` | u64? | From the registry; absent for on-disk-only snapshots. |
| `memory_logical_bytes` | u64 | `stat().st_size` of `memory.bin`. |
| `memory_physical_bytes` | u64 | `st_blocks × 512` — meaningful on reflink filesystems. |
| `vmstate_bytes` | u64 | Size of the vmstate file. |
| `parent_tag` | string? | Direct parent in the v0.5 chain; absent for bases. |
| `parent_content_hash` | string? | SHA-256 of the parent's `memory.bin` at chain-build time. |
| `chain_depth` | u32 | Number of diff links between this tag and its chain root (0 for a base). |
| `ancestors` | string[] | Root → direct parent. Empty for bases. |
| `dependents` | string[] | Tags whose `parent_tag` is this tag. The `rmi`-orphan set. |

## ErrorBody

Every 4xx and 5xx response carries:

```json
{ "error": "human-readable message" }
```
