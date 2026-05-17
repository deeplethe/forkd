#!/usr/bin/env bash
# Build a forkd parent rootfs containing the LangGraph-style ReAct
# agent demo (see README.md for the pitch).
#
# Output: ./parent.ext4 — a writable ext4 rootfs sized for python +
# the demo agent. ~700 MiB.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

IMAGE="${IMAGE:-python:3.12-slim}"
SIZE_MIB="${SIZE_MIB:-1024}"
OUT="$SCRIPT_DIR/parent.ext4"

[ "$(id -u)" -eq 0 ] || { echo "run as root" >&2; exit 1; }

echo "==> building base rootfs from $IMAGE"
bash "$REPO_ROOT/scripts/build-rootfs.sh" "$IMAGE" "$OUT" "$SIZE_MIB" python3 python3-pip ca-certificates

# Now install the demo agent and its pinned deps into the rootfs.
# We mount the ext4 image, copy files, pip install in a chroot.
MNT="$(mktemp -d /tmp/forkd-langgraph-mnt-XXXXX)"
trap 'umount -l "$MNT" 2>/dev/null || true; rm -rf "$MNT"' EXIT

echo "==> mounting rootfs to install demo agent"
mount -o loop "$OUT" "$MNT"

echo "==> copying agent files into /opt/forkd-demo/"
install -d -m 0755 "$MNT/opt/forkd-demo"
install -m 0644 "$SCRIPT_DIR/agent.py" "$MNT/opt/forkd-demo/agent.py"
install -m 0644 "$SCRIPT_DIR/tools.py" "$MNT/opt/forkd-demo/tools.py"
install -m 0644 "$SCRIPT_DIR/requirements.txt" "$MNT/opt/forkd-demo/requirements.txt"

echo "==> pip install (chroot)"
# Use systemd-resolved DNS if available; fall back to public DNS for
# the chroot pip step.
install -m 0644 /etc/resolv.conf "$MNT/etc/resolv.conf" 2>/dev/null || true

# Mount /proc, /sys, /dev so pip's subprocess machinery works.
for d in proc sys dev; do
  mount --bind /$d "$MNT/$d" 2>/dev/null || true
done

chroot "$MNT" /bin/sh -c '
  set -eux
  python3 -m pip install --no-cache-dir --upgrade pip
  python3 -m pip install --no-cache-dir -r /opt/forkd-demo/requirements.txt
'

# Tear down bind mounts before unmounting the loop.
for d in proc sys dev; do
  umount "$MNT/$d" 2>/dev/null || true
done

echo
echo "==> rootfs ready: $OUT ($(du -h "$OUT" | cut -f1))"
echo
echo "next:"
echo "  forkd snapshot --tag langgraph --kernel <vmlinux> --rootfs $OUT --rw --tap forkd-tap0 --boot-wait-secs 20"
