#!/usr/bin/env bash
# Build a forkd parent rootfs from the canonical Jupyter SciPy image.
#
# The base image is ~3 GB (full SciPy stack preinstalled). Plan for
# a larger memory.bin after warm-up (~700 MiB) — more pages get
# touched during the snapshot warm-up phase than with python-numpy/.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

IMAGE="${IMAGE:-quay.io/jupyter/scipy-notebook:latest}"
SIZE_MIB="${SIZE_MIB:-4096}"
OUT="$SCRIPT_DIR/parent.ext4"

[ "$(id -u)" -eq 0 ] || { echo "run as root" >&2; exit 1; }

echo "==> building rootfs from $IMAGE (this is a ~3 GB image; first pull may take several minutes)"
bash "$REPO_ROOT/scripts/build-rootfs.sh" "$IMAGE" "$OUT" "$SIZE_MIB"

echo
echo "parent rootfs ready: $OUT ($(du -h "$OUT" | cut -f1))"
echo
echo "next:"
echo "  sudo forkd snapshot --tag jk --kernel <vmlinux> --rootfs $OUT \\"
echo "      --tap forkd-tap0 --boot-wait-secs 20"
echo
echo "tip: --boot-wait-secs 20 gives ipython + SciPy time to fully import"
echo "into the parent before snapshot; lower values miss some warm-up."
