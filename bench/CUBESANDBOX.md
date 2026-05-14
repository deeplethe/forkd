# CubeSandbox bench methodology

## Host (read this first if you suspect nested virtualisation)

Both forkd and CubeSandbox were measured on the same **bare-metal**
host. There is **no nested virtualisation** in this setup:

```
$ systemd-detect-virt
none
$ grep "model name" /proc/cpuinfo | head -1
model name : 12th Gen Intel(R) Core(TM) i7-12700
$ grep -o vmx /proc/cpuinfo | head -1
vmx
```

12th-gen Intel Core, VT-x available directly, Ubuntu 24.04 / Linux 6.14
running on the metal. Every microVM in either project is host → L1
KVM guest, same level for both. CubeSandbox was **not** run inside a
dev-env VM or any other intermediate hypervisor; the one-click install
script targets the host directly (see "Setup" below).

## Result

CubeSandbox N=100 spawn measured at **20,304 ms** on the same dev box
forkd was measured on (Ubuntu 24.04 / Linux 6.14 / 20 vCPU / 30 GiB /
KVM). **77 of 100** sandboxes spawned cleanly; the rest hit
`newExt4RawByReflinkCopy failed: e2fsck 1.47.0 (5-Feb-2023): bad magic
number in superblock` under concurrent load. The wall-clock figure is
the full N=100 run including the failed-spawn rollbacks.

## Setup

```bash
# CubeSandbox v0.2.0 one-click install with custom ports.
# Patches applied on this host (1Panel-occupied default ports):
#   CubeMaster/conf.yaml — replace 127.0.0.1:3306 → :13306
#   CubeMaster/conf.yaml — replace 127.0.0.1:6379 → :16379
sudo bash /opt/cube-stage/cube-sandbox-one-click-9c16021/install.sh
# After install, port + service patches above, then:
sudo /usr/local/services/cubetoolbox/scripts/one-click/up.sh

# Build a template once (cached afterwards):
cubemastercli template create-from-image \
    --image python:3.12-slim \
    --template-id forkd-bench-pynp \
    --writable-layer-size 2Gi \
    --allow-internet-access
```

The cube-api listens on port `6000` (we overrode `CUBE_API_BIND`).

## Workload

`bench/cube-bench.py` (see [`compare-all.py`](./compare-all.py))
issues N concurrent `POST /sandboxes {"templateID":"forkd-bench-pynp"}`
via the cube-api REST endpoint, then `DELETE /sandboxes/:id` per
successful spawn. The numpy import workload runs inside each
sandbox but most fail before they get there because of the storage
issue noted below.

## Why success rate is < 100% on this host

Under concurrent load, cubelet's `newExt4RawByReflinkCopy` path
sometimes produces an ext4 image whose superblock fails `e2fsck`.
The XFS filesystem hosting `/data/cubelet` has `reflink=1` enabled
(verified with `xfs_info`) and the host has 30 GiB free, so this is
not a filesystem feature or disk-space issue — it looks like a
contention bug in cubelet's parallel reflink-copy path.

A second N=100 run measured 20,304 ms / 77 succeeded; the first run
measured 19,788 ms / 36 succeeded. Wall-clock is stable; success
rate is variable. The chart row uses the more recent figure.

## Notes

Tencent's published numbers ("<60 ms" cold-start, "<150 ms under
concurrent") would put CubeSandbox ahead of forkd on raw cold-start.
On the specific Ubuntu 24.04 / Linux 6.14 / 20-vCPU host we tested,
the storage path was the bottleneck, not VM boot. A cleaner host (no
1Panel co-tenancy, dedicated XFS partition for `/data/cubelet`) is
likely to give CubeSandbox a substantially better number.

## Upstream response (2026-05-14)

We filed the methodology + the reflink-copy race upstream:
[TencentCloud/CubeSandbox#235](https://github.com/TencentCloud/CubeSandbox/issues/235).
The maintainer's response confirmed two things that recontextualise
the numbers above:

1. **The race is on a slow code path the original template
   inadvertently selected.** CubeSandbox pre-formats a pool of
   writable-layer ext4 images at sizes listed in
   `pool_default_format_size_list` (default `["1Gi"]`). A sandbox
   whose `writable_layer_size` matches one of those sizes reuses a
   pool entry — fast path, no `mkfs.ext4` or reflink-copy per
   sandbox. We passed `--writable-layer-size 2Gi`, which doesn't
   match the default pool, so every sandbox went through the live
   `mkfs.ext4 + reflink-copy` slow path. That's where the bad-magic
   race lives.
2. **Cube's published N=50/N=100 numbers are measured on a 96 vCPU
   server.** A 20 vCPU host (this dev box) is outside their tested
   matrix. Per the maintainer: P99 under 200 ms at N=100 on a
   96-vCPU node.

Cube also accepted the first two improvements from our issue (a
configurable `cmdTimeout`, and richer diagnostic info on
`newExt4RawByReflinkCopy` failures) and is reviewing the third
(drop per-clone `e2fsck`).

## Small-N replay on the same (slow-path) configuration

After the upstream exchange we re-ran with the same 2 GiB template
at smaller N — staying on the slow path so the comparison is
apples-to-apples with the N=100 row, but small enough to fit the
30 GiB host RAM budget (template spec = 2 GiB per sandbox →
max ~14 concurrent).

Script: [`bench/cube-replay.sh`](./cube-replay.sh).

| N | Succeeded | Wall-clock | Per-sandbox |
|---:|:---:|---:|---:|
| 1 | 1/1 | 924 ms | 924 ms |
| 5 | 5/5 | 2,207 ms | 441 ms |
| 10 | 10/10 | 2,567 ms | 257 ms |

Observations:

- **100 % success rate at every size we measured.** The reflink-copy
  race only fired at N=100 with the 2 GiB writable layer; smaller N
  hit no failures.
- Single-instance cold start ≈ **924 ms** here, vs Cube's published
  fast-path **<60 ms**. The ~15× gap is the combined cost of the
  slow path (live `mkfs.ext4` plus reflink-copy of a 2 GiB image)
  and the host being well outside their 96 vCPU testing matrix.
- Per-sandbox cost shrinks substantially with concurrency
  (924 → 441 → 257 ms / sandbox) — pipelined work the original
  20.3 s / 100 = 203 ms-per-sandbox number is consistent with.

What we did **not** measure here: the fast path
(`writable_layer_size` matching `pool_default_format_size_list`).
Doing so would require either a new template with a 1 GiB writable
layer or reconfiguring the pool for 2 GiB; we left it for whenever
either Cube or a downstream user wants a head-to-head fast-path
number on this host.
