# Operator runbook

How to bring up a sandstorm controller, what to watch in production, and
how to recover from common failure modes.

---

## 1. Bring-up

### Host prerequisites

- x86_64 Linux, kernel 5.10+ (5.20+ recommended for free RNG re-seed)
- KVM available (`/dev/kvm` present, your user in the `kvm` group)
- Firecracker binary v1.7+ on the `$PATH`
- cgroup v2 unified hierarchy (`mount -t cgroup2`)
- iproute2

### One-shot install

```bash
sudo bash scripts/setup-host.sh    # KVM, Firecracker, KSM tuning
sudo bash scripts/netns-setup.sh 100   # provision N child netns
cargo build --release
sudo install -m 0755 target/release/sandstorm-controller /usr/local/bin/
sudo install -m 0755 target/release/sandstorm            /usr/local/bin/
sudo install -m 0644 packaging/systemd/sandstorm-controller.service /etc/systemd/system/
sudo mkdir -p /etc/sandstorm /var/lib/sandstorm /var/log/sandstorm
sudo bash -c 'head -c 32 /dev/urandom | base64 > /etc/sandstorm/token'
sudo chmod 600 /etc/sandstorm/token

# Optional but recommended for any non-loopback bind: TLS.
# Drop a cert.pem + key.pem (Let's Encrypt or your internal CA) into
# /etc/sandstorm/tls/ and add --tls-cert / --tls-key to the systemd ExecStart.

sudo systemctl daemon-reload
sudo systemctl enable --now sandstorm-controller
```

Verify:

```bash
curl http://127.0.0.1:8889/healthz
# {"ok":true}

curl http://127.0.0.1:8889/metrics
# sandstorm_sandboxes_active 0
```

---

## 2. Daily operations

### Auth

The daemon's token lives in `/etc/sandstorm/token`. Rotate by writing a
new value and restarting the daemon (`systemctl restart
sandstorm-controller`). Existing sandboxes survive the restart; in-flight
HTTP requests do not.

### Metrics

Scrape `:8889/metrics` from Prometheus. Names that should always
exist: `sandstorm_snapshots_total`, `sandstorm_sandboxes_active`,
`sandstorm_build_info`.

Suggested alerts:

- `sandstorm_sandboxes_active` > 80% of host vCPU count for 5 min
- absence of `sandstorm_build_info` for 1 min → daemon down

### Audit log

`/var/log/sandstorm/audit.log`, one JSON object per line:

```json
{"ts":"2026-05-12T07:12:34Z","method":"POST","path":"/v1/sandboxes","status":201,"latency_us":98342,"ua":"sandstorm-cli/0.1"}
```

Rotate with `logrotate`. The packaged policy renames the current file,
creates a new `0600` log, and runs `systemctl try-reload-or-restart
sandstorm-controller`. The service maps reload to `SIGHUP`; the daemon flushes
the old writer and atomically reopens the configured path without restarting
or dropping in-flight requests. The `try-` variant safely falls back to a
restart for an older installed service without reload support and does nothing
when the service is inactive. A successful reopen writes a `log_reopened`
event to the new file. For a manual rotation, send `systemctl reload
sandstorm-controller` after moving the file.

---

## 3. Failure modes

### Daemon won't start: "bind 127.0.0.1:8889" fails

Another process is already on that port. `ss -ltnp | grep 8889` to find
it. Stale daemon? `pkill -f sandstorm-controller` then `systemctl start`.

### `POST /v1/sandboxes` returns 500 "restore_many: ..."

Usually one of:

- The snapshot was created against a different kernel than what's
  available on this host. Re-create with the current kernel.
- Out of disk on `/tmp` — each child needs ~5 MiB of work-dir space.
- Out of memory — host has hit `memory.max` on `/sys/fs/cgroup/sandstorm/`
  or has no free RAM. Tune `memory_limit_mib` per child.

### Children are alive but `exec`/`eval` times out

Network namespace mismatch. With `per_child_netns: true`, the agent is
reachable only from inside its netns; the daemon does the `setns(2)`
dance for you, but the host must have `/var/run/netns/sandstorm-child-<i>`
provisioned via `scripts/netns-setup.sh N`. Re-run that script if you've
restarted networking.

### Reconcile pruned all my sandboxes after restart

The daemon checks `/proc/<pid>` for each registered sandbox's
Firecracker PID on startup. If the host rebooted (or the daemon's
state file outlived the FC processes), there's nothing to recover —
sandboxes don't survive host reboots. Create new ones from the
existing snapshots.

---

## 4. Capacity planning

On a 20-vCPU / 30 GiB host (the dev-bench config), sandstorm has been
exercised at N=200 children sharing one snapshot in ~750 ms wall-clock.
Sustained throughput depends on:

- KSM enabled and tuned (`scripts/setup-host.sh` sets sensible
  defaults)
- Snapshot's memory image size — smaller parents fork faster
- Whether you use `per_child_netns` (adds ~3 ms per child for the
  netns + agent probe round-trip)

For your own host, run `bench/bench-spawn-100.sh` and look at the
sandstorm line in `bench/chart-spawn-100.png`.

---

## 5. Upgrading

1. Take the daemon down: `systemctl stop sandstorm-controller`. Live
   sandboxes get killed; persistent registry survives.
2. `cargo build --release && install -m 0755 target/release/sandstorm-controller /usr/local/bin/`
3. `systemctl start sandstorm-controller`. On startup, `reconcile()` prunes
   sandbox entries whose Firecracker PID is gone.
4. Verify `/version` reports the new build.
