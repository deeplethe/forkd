#!/usr/bin/env bash
# Build a sandstorm parent rootfs with Python 3.12 + numpy preinstalled.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

IMAGE="python:3.12-slim"
SIZE_MIB="${SIZE_MIB:-1536}"
OUT="$SCRIPT_DIR/parent.ext4"

[ "$(id -u)" -eq 0 ] || { echo "run as root" >&2; exit 1; }

bash "$REPO_ROOT/scripts/build-rootfs.sh" \
    "$IMAGE" \
    "$OUT" \
    "$SIZE_MIB" \
    "python3-numpy"

echo
echo "parent rootfs ready: $OUT ($(du -h "$OUT" | cut -f1))"
echo "next: sudo sandstorm snapshot --tag pyagent --kernel <vmlinux> --rootfs $OUT --tap sandstorm-tap0"
