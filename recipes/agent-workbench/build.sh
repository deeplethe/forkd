#!/usr/bin/env bash
# Build a sandstorm parent rootfs from agent-infra/sandbox.
#
# This recipe trades a much larger memory image (~5 GB rootfs, ~1 GB
# memory.bin after warm-up) for batteries-included tooling. Plan for
# higher per-fork memory cost when fanning out.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

IMAGE="${IMAGE:-ghcr.io/agent-infra/sandbox:latest}"
SIZE_MIB="${SIZE_MIB:-6144}"
OUT="$SCRIPT_DIR/parent.ext4"

[ "$(id -u)" -eq 0 ] || { echo "run as root" >&2; exit 1; }

echo "==> building rootfs from $IMAGE (this is a ~5 GB image; pulling may take a few minutes)"
bash "$REPO_ROOT/scripts/build-rootfs.sh" "$IMAGE" "$OUT" "$SIZE_MIB"

echo
echo "parent rootfs ready: $OUT ($(du -h "$OUT" | cut -f1))"
echo
echo "next: sudo sandstorm snapshot --tag wb --kernel <vmlinux> --rootfs $OUT --tap sandstorm-tap0"
echo
echo "tip: agent-workbench has services that take a few seconds to settle;"
echo "use --boot-wait-secs 30 on the snapshot command for a clean warm-up."
