#!/usr/bin/env bash
# Build a sandstorm parent rootfs from node:22-slim.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

IMAGE="${IMAGE:-node:22-slim}"
SIZE_MIB="${SIZE_MIB:-512}"
OUT="$SCRIPT_DIR/parent.ext4"

[ "$(id -u)" -eq 0 ] || { echo "run as root" >&2; exit 1; }

bash "$REPO_ROOT/scripts/build-rootfs.sh" "$IMAGE" "$OUT" "$SIZE_MIB"

echo
echo "parent rootfs ready: $OUT ($(du -h "$OUT" | cut -f1))"
echo "next: sudo sandstorm snapshot --tag node --kernel <vmlinux> --rootfs $OUT --tap sandstorm-tap0"
