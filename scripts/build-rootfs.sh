#!/usr/bin/env bash
# build-rootfs.sh — create a writable ext4 rootfs from a Docker image,
# with extra apt packages pre-installed so guests boot with deps warm on disk.
#
# Output: $OUTPUT (default: ./rootfs.ext4) — bootable Linux rootfs.
#
# Usage:
#   build-rootfs.sh [image] [output] [size_mb] [extra_pkgs...]
# Example:
#   build-rootfs.sh ubuntu:24.04 python-rootfs.ext4 2048 python3 python3-numpy
#
# Requires: docker, sudo, mkfs.ext4 (e2fsprogs).
#
# Why not unsquashfs:
#   Ubuntu's squashfs-tools package depends on bzip2 which has been
#   broken in our apt cache. Docker is already installed and works.

set -euo pipefail

# Privilege shim: when already root (e.g. `forkd quickstart` in a
# container, or invoked via `sudo -E forkd parent build`), `sudo` may
# not be installed at all — run the privileged commands directly.
if [ "$(id -u)" -eq 0 ]; then
    SUDO=""
else
    SUDO="sudo"
fi

IMAGE="${1:-ubuntu:24.04}"
OUTPUT="${2:-rootfs.ext4}"
SIZE_MB="${3:-2048}"
shift 3 2>/dev/null || shift $#
EXTRA_PKGS=("$@")

WORK="$(mktemp -d /tmp/forkd-rootfs-XXXXX)"
CONTAINER="forkd-rootfs-$$"

say() { printf "\033[1;34m==>\033[0m %s\n" "$*"; }
die() { printf "\033[1;31merror:\033[0m %s\n" "$*" >&2; cleanup; exit 1; }

cleanup() {
    $SUDO umount "$WORK/dev"  2>/dev/null || true
    $SUDO umount "$WORK/sys"  2>/dev/null || true
    $SUDO umount "$WORK/proc" 2>/dev/null || true
    docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
    $SUDO rm -rf "$WORK" 2>/dev/null || true
}
trap cleanup EXIT

command -v docker      >/dev/null || die "docker not found"
command -v mkfs.ext4   >/dev/null || die "mkfs.ext4 not found"

say "image:      $IMAGE"
say "output:     $OUTPUT (${SIZE_MB} MiB)"
say "extra pkgs: ${EXTRA_PKGS[*]:-none}"
say "work dir:   $WORK"

# ----------------------------------------------------------------------------
say "[1/5] pulling + creating container..."
docker pull -q "$IMAGE"
docker create --name "$CONTAINER" "$IMAGE" /bin/true >/dev/null

# ----------------------------------------------------------------------------
say "[2/5] exporting container filesystem to $WORK..."
mkdir -p "$WORK"
docker export "$CONTAINER" | $SUDO tar -xf - -C "$WORK"
$SUDO du -sh "$WORK"

# ----------------------------------------------------------------------------
if [ "${#EXTRA_PKGS[@]}" -gt 0 ]; then
    say "[3/5] chroot apt install: ${EXTRA_PKGS[*]}"

    # bring up host DNS + bind /proc /sys /dev for apt to work
    $SUDO cp /etc/resolv.conf "$WORK/etc/resolv.conf"
    $SUDO mount --bind /proc "$WORK/proc"
    $SUDO mount --bind /sys  "$WORK/sys"
    $SUDO mount --bind /dev  "$WORK/dev"

    $SUDO chroot "$WORK" /bin/bash -e <<EOF
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y --no-install-recommends ${EXTRA_PKGS[*]} 2>&1 | tail -5
# Trim caches to shrink image
apt-get clean
rm -rf /var/lib/apt/lists/* /var/cache/apt/archives/*
EOF

    $SUDO umount "$WORK/dev"  || true
    $SUDO umount "$WORK/sys"  || true
    $SUDO umount "$WORK/proc" || true
else
    say "[3/5] skipping apt install (no extra pkgs requested)"
fi

# ----------------------------------------------------------------------------
say "[4/5] installing forkd init + agent..."
# Copy the init script and the Python agent into the rootfs.
INIT_SRC="$(dirname "$(readlink -f "$0")")/../rootfs-init"
if [ -d "$INIT_SRC" ]; then
    $SUDO cp "$INIT_SRC/forkd-init.sh"  "$WORK/forkd-init.sh"
    $SUDO cp "$INIT_SRC/forkd-agent.py" "$WORK/forkd-agent.py"
    $SUDO chmod 755 "$WORK/forkd-init.sh" "$WORK/forkd-agent.py"
    say "    installed /forkd-init.sh and /forkd-agent.py"
else
    say "    rootfs-init/ not found at $INIT_SRC — guest will boot without forkd agent"
fi
# Empty root password for development convenience.
$SUDO chroot "$WORK" /bin/bash -c "passwd -d root 2>/dev/null || true"

# ----------------------------------------------------------------------------
say "[5/5] building ext4 image ($SIZE_MB MiB)..."
dd if=/dev/zero of="$OUTPUT" bs=1M count="$SIZE_MB" status=progress 2>&1 | tail -1
mkfs.ext4 -q -F -L forkd-rootfs -d "$WORK" "$OUTPUT"
ls -lh "$OUTPUT"

echo
say "done. Try:"
echo "  forkd snapshot --tag python --kernel <vmlinux> --rootfs $(realpath "$OUTPUT")"
echo "  forkd fork --tag python -n 100"
