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

# Privilege shim: when already root (e.g. `sandstorm quickstart` in a
# container, or invoked via `sudo -E sandstorm parent build`), `sudo` may
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

WORK="$(mktemp -d /tmp/sandstorm-rootfs-XXXXX)"
CONTAINER="sandstorm-rootfs-$$"

say() { printf "\033[1;34m==>\033[0m %s\n" "$*"; }
die() { printf "\033[1;31merror:\033[0m %s\n" "$*" >&2; cleanup; exit 1; }

cleanup() {
    for mnt in dev sys proc; do
        $SUDO umount -l "$WORK/$mnt" 2>/dev/null || true
    done
    docker rm -f "$CONTAINER" >/dev/null 2>&1 || true

    case "$WORK" in
        /tmp/sandstorm-rootfs-*) ;;
        *)
            say "warning: refusing to remove unexpected work dir: $WORK"
            return
            ;;
    esac
    for mnt in dev sys proc; do
        if mountpoint -q "$WORK/$mnt" 2>/dev/null; then
            say "warning: refusing to remove $WORK; $WORK/$mnt is still mounted"
            return
        fi
    done
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
# Skip the registry pull when the image already exists locally — e.g. a
# recipe-built wrapped image (coding-agent tags one as
# sandstorm-coding-agent:tmp-$$). Pulling such a local-only tag forces a
# needless registry round-trip that 429s behind a throttled mirror and
# aborts the build.
docker image inspect "$IMAGE" >/dev/null 2>&1 || docker pull -q "$IMAGE"
docker create --name "$CONTAINER" "$IMAGE" /bin/true >/dev/null

# ----------------------------------------------------------------------------
say "[2/5] exporting container filesystem to $WORK..."
mkdir -p "$WORK"
docker export "$CONTAINER" | $SUDO tar -xf - -C "$WORK"
$SUDO du -sh "$WORK"

# Materialize the Docker image's Config.Env PATH into /etc/environment.
# docker export gives us the filesystem contents but NOT the image's
# configured environment variables.  For images like rust:latest or
# golang:1.25, tool directories (e.g. /usr/local/cargo/bin,
# /usr/local/go/bin) exist only in Config.Env, not in /etc/environment.
# Without this step, the guest agent's _load_container_env() would find
# no PATH in /etc/environment and fall back to a default that misses
# tool-specific directories, causing "command not found" for cargo, go, etc.
IMAGE_PATH=$(docker inspect --format '{{range .Config.Env}}{{println .}}{{end}}' "$CONTAINER" 2>/dev/null \
    | grep '^PATH=' | tail -1 | cut -d= -f2- || true)
if [ -n "$IMAGE_PATH" ]; then
    say "    materializing Docker PATH into /etc/environment"
    # Defense-in-depth: break symlinks before writing (a malicious image
    # could symlink /etc/environment to a host file).
    [ -L "$WORK/etc/environment" ] && $SUDO rm -f "$WORK/etc/environment"
    $SUDO touch "$WORK/etc/environment"
    $SUDO sed -i '/^PATH=/d' "$WORK/etc/environment" 2>/dev/null || true
    echo "PATH=$IMAGE_PATH" | $SUDO tee -a "$WORK/etc/environment" >/dev/null
else
    say "    no PATH found in Docker Config.Env, keeping existing /etc/environment"
fi

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
say "[4/5] installing sandstorm init + agent..."
# Copy the init script and the Python agent into the rootfs.
INIT_SRC="$(dirname "$(readlink -f "$0")")/../rootfs-init"
if [ -d "$INIT_SRC" ]; then
    $SUDO cp "$INIT_SRC/sandstorm-init.sh"  "$WORK/sandstorm-init.sh"
    $SUDO cp "$INIT_SRC/sandstorm-agent.py" "$WORK/sandstorm-agent.py"
    $SUDO chmod 755 "$WORK/sandstorm-init.sh" "$WORK/sandstorm-agent.py"
    say "    installed /sandstorm-init.sh and /sandstorm-agent.py"
else
    say "    rootfs-init/ not found at $INIT_SRC — guest will boot without sandstorm agent"
fi
# Empty root password for development convenience.
$SUDO chroot "$WORK" /bin/bash -c "passwd -d root 2>/dev/null || true"

# ----------------------------------------------------------------------------
say "[5/5] building ext4 image ($SIZE_MB MiB)..."
# Build a sparse ext4 file. Use truncate (not dd) so the file is
# sparse — physical disk usage is proportional to actual written
# content, not the nominal size. This lets us use a generous default
# size (24 GiB) for engineering workloads without wasting disk on
# ephemeral function-call sandboxes.
#
# Build to a temporary sibling then atomically rename on success, so
# a failed mkfs does not destroy a prior cache artifact: truncate -s
# in-place on an existing fully-allocated file does not release its
# blocks, and if mkfs fails the old file is already corrupted.
TMP_OUTPUT="${OUTPUT}.tmp.$$"
truncate -s "${SIZE_MB}M" "$TMP_OUTPUT"
if ! mkfs.ext4 -q -F -L sandstorm-rootfs -d "$WORK" "$TMP_OUTPUT"; then
    rm -f "$TMP_OUTPUT"
    die "mkfs.ext4 failed"
fi
mv "$TMP_OUTPUT" "$OUTPUT"
ls -lh "$OUTPUT"

echo
say "done. Try:"
echo "  sandstorm snapshot --tag python --kernel <vmlinux> --rootfs $(realpath "$OUTPUT")"
echo "  sandstorm fork --tag python -n 100"
