#!/usr/bin/env bash
set -euo pipefail

script="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/build-rootfs.sh"

# Issue #264: cleanup must not rm -rf while bind mounts are still below $WORK.
grep -F 'umount -l "$WORK/$mnt"' "$script" >/dev/null
grep -F 'mountpoint -q "$WORK/$mnt"' "$script" >/dev/null
# Also refuse unexpected rm targets before running sudo rm -rf.
grep -F '/tmp/forkd-rootfs-' "$script" >/dev/null
