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
