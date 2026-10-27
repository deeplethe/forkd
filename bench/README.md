# Benchmarks

How to reproduce the numbers in the top-level [README](../README.md).

## Workload

Spawn 100 sandboxes, each ready to execute
`numpy.zeros(5).tolist()`. Measure wall-clock from the first sandbox
request to the last sandbox confirming the result.

All numbers in the published charts are measured on the same host:

- Ubuntu 24.04
- Linux 6.14
- 20 vCPU
- 30 GiB RAM
- KVM enabled

## Files

| File | Purpose |
|---|---|
| `bench-spawn-100.sh` | End-to-end harness: builds rootfs, snapshots a parent, runs the bench across sandstorm / Docker / gVisor / Firecracker, writes results as JSON |
| `compare-all.py` | Python driver used by the harness; talks to each backend through the same `import numpy` workload |
| `compare-vs-docker.sh` | Smaller harness comparing sandstorm vs Docker vs fresh-Firecracker only |
| `generate_charts.py` | Renders `chart-spawn-100.png` and `chart-memory-per.png` from `BENCH_RESULTS=$file.json` (or the baseline data baked into the script) |
| `CUBESANDBOX.md` | Methodology note for the CubeSandbox row in the spawn-time chart |

## Reproducing

```bash
# 1. Build the rootfs + snapshot once.
sudo bash scripts/build-rootfs.sh python:3.12-slim python-rootfs.ext4 1536 python3-numpy
sudo bash scripts/host-tap.sh
sudo bash scripts/netns-setup.sh 100
sudo sandstorm snapshot --tag pyagent \
    --kernel ./vmlinux-6.1.141 --rootfs ./python-rootfs.ext4 --tap sandstorm-tap0

# 2. Run the harness, write a JSON file.
sudo -E bash bench/bench-spawn-100.sh > /tmp/results.json

# 3. Re-render charts from the new measurements.
BENCH_RESULTS=/tmp/results.json python3 bench/generate_charts.py
```

Each backend in the harness can be enabled/disabled with
`--backends sandstorm|docker|gvisor|firecracker|cubesandbox|boxlite|opensandbox`
on `compare-all.py`.

### Backend prerequisites

- **sandstorm** — pre-existing tagged snapshot (`sandstorm snapshot --tag pyagent ...`)
  and per-child netns provisioned (`scripts/netns-setup.sh N`).
- **docker** — Docker daemon running, `python:3.12-slim` image pullable.
- **boxlite** — `pip install boxlite==0.9.3` (Python SDK is sufficient;
  the optional `cargo install boxlite-cli` step needs `protoc` and is
  skipped for benchmarking). Measurement notes in [`BOXLITE.md`](./BOXLITE.md).
- **opensandbox** — `pip install opensandbox==0.1.8` plus a running
  `uvx opensandbox-server`. Notes in [`OPENSANDBOX.md`](./OPENSANDBOX.md).
- **cubesandbox** — `cube-sandbox-one-click v0.2.0` installed on the
  same dev box; conf.yaml port overrides documented in
  [`CUBESANDBOX.md`](./CUBESANDBOX.md).
- **gvisor** — `runsc` registered as a Docker runtime.

## Notes

- Numbers vary across CPUs, kernels, and KSM tuning. The order of
  magnitude is reproducible; the exact ms count for a given backend
  on your host won't be.
- Docker and gVisor numbers include the cold `import numpy` per
  sandbox; that's the fairness budget — sandstorm skips it because the
  parent already imported numpy before being snapshotted.
