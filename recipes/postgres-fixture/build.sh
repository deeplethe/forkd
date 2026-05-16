#!/usr/bin/env bash
# Build a forkd parent rootfs from postgres:16-alpine, with the
# database initialised and the postmaster pre-launched. Children
# forked from the snapshot inherit a ready-to-query postgres in
# ~10 ms instead of ~2 s for a fresh container start + initdb.
#
# The fork-per-test model: every child gets an isolated, identically-
# seeded database, all sharing the parent's resident memory via CoW.
# Tests that mutate the schema or write rows diverge only on the
# pages they actually touch; teardown is "kill the child VM".

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

IMAGE="${IMAGE:-postgres:16}"
SIZE_MIB="${SIZE_MIB:-1536}"
OUT="$SCRIPT_DIR/parent.ext4"

# python3 is required by forkd-init.sh to launch forkd-agent.py inside
# the guest. The Debian-based postgres:16 image doesn't ship it; we
# add it via build-rootfs.sh's extra-apt-packages mechanism. (Alpine
# variants are unsupported because build-rootfs.sh uses apt.)
EXTRA_PKGS="${EXTRA_PKGS:-python3}"

# Database credentials for the parent. Children inherit these; tests
# typically connect with these values and don't care about the password
# since the postgres port is only reachable inside the child netns.
PG_USER="${PG_USER:-forkd}"
PG_PASSWORD="${PG_PASSWORD:-forkd}"
PG_DATABASE="${PG_DATABASE:-forkd_test}"

[ "$(id -u)" -eq 0 ] || { echo "run as root" >&2; exit 1; }

echo "==> building rootfs from $IMAGE (+ extra: $EXTRA_PKGS)"
bash "$REPO_ROOT/scripts/build-rootfs.sh" "$IMAGE" "$OUT" "$SIZE_MIB" $EXTRA_PKGS

# Mount the rootfs and inject:
# 1. A warmup script that runs initdb (once) + starts the postmaster,
#    emits the line-JSON ready handshake, then idles. forkd-agent.py
#    will spawn this via FORKD_WARMUP_CMD before the snapshot is taken.
# 2. /etc/forkd-recipe.env declaring the warmup command and recipe
#    metadata.
ROOTFS_MNT=$(mktemp -d)
mount -o loop "$OUT" "$ROOTFS_MNT"
cleanup() {
    umount "$ROOTFS_MNT" 2>/dev/null || true
    rmdir "$ROOTFS_MNT" 2>/dev/null || true
}
trap cleanup EXIT

echo "==> injecting postgres warmup hook"

mkdir -p "$ROOTFS_MNT/var/lib/postgresql/data"
chmod 700 "$ROOTFS_MNT/var/lib/postgresql/data"
chown 70:70 "$ROOTFS_MNT/var/lib/postgresql/data"  # alpine postgres uid

cp "$SCRIPT_DIR/forkd-pg-warmup" "$ROOTFS_MNT/usr/local/bin/forkd-pg-warmup"
chmod 755 "$ROOTFS_MNT/usr/local/bin/forkd-pg-warmup"

cat > "$ROOTFS_MNT/etc/forkd-recipe.env" <<EOF
# Tells forkd-agent.py to spawn the postgres warmup before serving.
FORKD_WARMUP_CMD=/usr/local/bin/forkd-pg-warmup
FORKD_RECIPE=postgres-fixture
PG_USER=$PG_USER
PG_DATABASE=$PG_DATABASE
EOF

echo
echo "parent rootfs ready: $OUT ($(du -h "$OUT" | cut -f1))"
echo
echo "next:"
echo "  sudo -E forkd snapshot --tag pgfix \\"
echo "      --kernel ./vmlinux-6.1.141 \\"
echo "      --rootfs $OUT \\"
echo "      --tap forkd-tap0 \\"
echo "      --boot-wait-secs 15      # let initdb + postmaster settle"
echo
echo "fork-per-test:"
echo "  sudo bash scripts/netns-setup.sh 20"
echo "  sudo -E forkd fork --tag pgfix -n 20 --per-child-netns --memory-limit-mib 512"
echo
echo "each child accepts psql at 10.42.0.2:5432 (user=$PG_USER, db=$PG_DATABASE)"
