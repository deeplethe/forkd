# forkd on Kubernetes

A starter manifest for running forkd-controller as a Pod.

> **Status: API surface verified end-to-end.** Deployed `forkd-controller`
> on a single-node **k3s v1.35** cluster on bare-metal Ubuntu 24.04 /
> Linux 6.14 / KVM; pod reaches `Running`, `/healthz` returns 200,
> bearer-token auth works against `/version` / `/v1/snapshots` /
> `/v1/sandboxes` / `/metrics`. Actually forking VMs from inside the
> pod also requires a kernel image + parent rootfs reachable on the
> node — see the [Quick start](#quick-start) for that.

The model is **one controller Pod hosts N sandbox children** — the K8s
scheduler runs once at Pod creation regardless of fan-out, unlike
Kata / Cube / Firecracker-on-K8s designs that schedule one Pod per
sandbox.

## Quick start

1. Build or pull the controller image:

   ```bash
   # If building locally:
   docker build -t ghcr.io/deeplethe/forkd-controller:latest .
   docker push ghcr.io/deeplethe/forkd-controller:latest
   ```

2. Generate a token and patch the Secret:

   ```bash
   TOKEN=$(head -c 32 /dev/urandom | base64)
   sed -i "s|REPLACE_ME_WITH_32_BYTES_BASE64|$TOKEN|" forkd-controller.yaml
   ```

3. Apply:

   ```bash
   kubectl apply -f forkd-controller.yaml
   kubectl -n forkd get pods -w
   ```

4. Smoke-test from inside the cluster:

   ```bash
   kubectl -n forkd port-forward svc/forkd-controller 8889:8889
   curl -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8889/v1/snapshots
   ```

## Requirements

- A Kubernetes node with **`/dev/kvm`** available and **VMX/SVM**
  enabled in BIOS. Bare-metal or hypervisor-with-nested-virt nodes
  qualify; managed Kubernetes (GKE/EKS/AKS) typically does **not**
  unless you pick a metal SKU or enable nested virt explicitly.
- **cgroup v2 unified hierarchy** on the host (the controller writes
  to `/sys/fs/cgroup/forkd/`).
- **Kernel image + parent rootfs** on the node, or mounted via a
  PersistentVolume the controller can read.

## Customisation

The starter manifest uses `privileged: true` for simplicity. For
tighter security:

- Swap for a **KVM device plugin** (e.g.
  [`kubevirt/kvm-device-plugin`](https://github.com/kubevirt/kubernetes-device-plugins))
  so the Pod gets `/dev/kvm` as a resource instead of via host mount.
- Drop `privileged: true` and keep only the capabilities you need
  (`NET_ADMIN` for tap setup, `SYS_ADMIN` for cgroup writes — review
  whether your kernel/runtime allows narrower).
- Replace `emptyDir` for `/var/lib/forkd` with a **PersistentVolumeClaim**
  so snapshots survive Pod restarts.

## What this manifest does NOT cover (yet)

- **DaemonSet shape** for multi-node deployments where you want one
  controller per node. Out of scope for v0.1 (forkd is single-host).
- **netns provisioning DaemonSet.** Per-child netns (`forkd-child-N`)
  needs `scripts/netns-setup.sh` run on each node before forks land.
  Wire as an init container or a separate DaemonSet depending on
  your platform.
- **HPA / autoscaling.** forkd's natural scale-out is "one controller
  per host"; horizontal autoscaling of the controller itself doesn't
  apply since each instance owns its own state. A future multi-node
  scheduler will deserve its own autoscaling shape.
- **NetworkPolicy.** The controller's port 8889 should be locked
  down to your agent backplane.

## Sizing

See the [Enterprise deployment FAQ](../../README.md#enterprise-deployment-faq)
for the per-pod sandbox capacity heuristic.
