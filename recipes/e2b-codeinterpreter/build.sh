#!/usr/bin/env bash
# Build a sandstorm parent rootfs from E2B's code-interpreter template.
#
# The image is published by e2bdev on Docker Hub; pulling it requires
# network access. ~600 MB on disk after extraction.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

IMAGE="${IMAGE:-e2bdev/code-interpreter:latest}"
SIZE_MIB="${SIZE_MIB:-1024}"
OUT="$SCRIPT_DIR/parent.ext4"

[ "$(id -u)" -eq 0 ] || { echo "run as root" >&2; exit 1; }

bash "$REPO_ROOT/scripts/build-rootfs.sh" \
    "$IMAGE" \
    "$OUT" \
    "$SIZE_MIB"

echo
echo "parent rootfs ready: $OUT ($(du -h "$OUT" | cut -f1))"
echo
echo "next:"
echo "  sudo sandstorm snapshot --tag ci --kernel <vmlinux> --rootfs $OUT --tap sandstorm-tap0"
echo
echo "tip: use the E2B-compatible Python SDK"
echo "  from sandstorm import Sandbox     # drop-in for: from e2b import Sandbox"
