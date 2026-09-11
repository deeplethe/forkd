#!/bin/bash
# /forkd-init.sh — PID 1 inside the guest. Mounts pseudo-fs, fixes
# DNS to public resolvers, then launches the Python agent.

mount -t proc proc /proc 2>/dev/null
mount -t sysfs sys /sys 2>/dev/null
mount -t devtmpfs devtmpfs /dev 2>/dev/null

# --- writable layer ---------------------------------------------------
# The rootfs is booted READ-ONLY (root=/dev/vda ro) and is one ext4 file
# shared by every sandbox restored from this snapshot, so nothing may
# write it: two guests writing one filesystem with no coordinator corrupt
# it ("Structure needs cleaning", foreign bytes inside package files).
# Everything writable is therefore provided here, in guest RAM.
#
# RAM is the point, not a limitation: tmpfs and overlayfs-upper pages are
# guest memory, and memory.bin is what a snapshot captures, so writable
# state survives a BRANCH exactly as the /tmp tmpfs always has.
#
#   plain tmpfs  /run /dev/shm      -- scratch that owns nothing
#   overlay      /etc /root /home   -- the image's baked content stays
#                /opt /srv           visible as the lower layer, writes
#                /usr/local /var     land in the tmpfs upper
#
# The overlay list is where jobs actually write (cargo registry and
# target, pnpm store, mix _build, the checkout itself) while keeping the
# image's preinstalled content — a bare tmpfs over those paths would hide
# it. /etc is on the list because the DNS fix below writes resolv.conf.
#
# A path that does not exist in the image is skipped, since a read-only
# root cannot be mkdir'd into. A write outside this set fails with EROFS:
# loud, and far better than silently mutating the shared base.
FORKD_RW_SIZE="$(grep -oE 'forkd\.rw_size=[^ ]+' /proc/cmdline | head -1 | cut -d= -f2-)"
[ -n "$FORKD_RW_SIZE" ] || FORKD_RW_SIZE=2g
if mount -t tmpfs -o "size=$FORKD_RW_SIZE" tmpfs /tmp 2>/dev/null; then
    echo "forkd-init: writable layer $FORKD_RW_SIZE (tmpfs at /tmp)"
else
    echo "forkd-init: WARN no writable tmpfs at /tmp; sandboxes will be read-only" >&2
fi
# Overlay upper/work dirs live on that tmpfs: overlayfs needs them on the
# same filesystem, and /tmp is the one path we know is writable.
mkdir -p /tmp/.rw/upper /tmp/.rw/work 2>/dev/null

_forkd_tmpfs() {  # <path>
    [ -d "$1" ] || return 0
    mount -t tmpfs -o "size=$FORKD_RW_SIZE" tmpfs "$1" 2>/dev/null || \
        echo "forkd-init: WARN $1 not writable (tmpfs mount failed)" >&2
}

_forkd_overlay() {  # <path>
    [ -d "$1" ] || return 0
    install -d "/tmp/.rw/upper$1" "/tmp/.rw/work$1" 2>/dev/null
    if mount -t overlay overlay \
        -o "lowerdir=$1,upperdir=/tmp/.rw/upper$1,workdir=/tmp/.rw/work$1" "$1" 2>/dev/null; then
        echo "forkd-init: $1 writable (overlay upper in RAM)"
    else
        echo "forkd-init: WARN $1 left read-only (overlay mount failed)" >&2
    fi
}

_forkd_tmpfs /run
_forkd_tmpfs /dev/shm
for _p in /etc /root /home /opt /srv /usr/local /var; do
    _forkd_overlay "$_p"
done

# Make sure PATH covers both Ubuntu (/usr/bin) and official python (/usr/local/bin)
# images. Subprocess invocations from the agent inherit this.
export PATH="/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"

# Persistent volumes — see VolumeSpec / BootConfig::with_volume on the host.
# The kernel cmdline carries an entry of the form:
#   forkd.mounts=vdb:/opt/cache,vdc:/var/cache/pip
# where each pair is "<device>:<guest mount path>".
mounts="$(grep -oE 'forkd\.mounts=[^ ]+' /proc/cmdline | head -1 | cut -d= -f2-)"
if [ -n "$mounts" ]; then
    IFS=',' read -ra _pairs <<<"$mounts"
    for pair in "${_pairs[@]}"; do
        dev="${pair%%:*}"
        target="${pair#*:}"
        if [ -z "$dev" ] || [ -z "$target" ] || [ "$dev" = "$target" ]; then
            echo "forkd-init: ignoring malformed mount entry '$pair'" >&2
            continue
        fi
        mkdir -p "$target"
        if ! mount "/dev/$dev" "$target" 2>/dev/null; then
            echo "forkd-init: WARN mount /dev/$dev -> $target failed" >&2
        fi
    done
fi

# Ubuntu Docker images symlink /etc/resolv.conf to a systemd-resolved
# stub that doesn't exist in our minimal init. Point to public resolvers
# so the guest can do DNS over the netns + host bridge NAT path.
rm -f /etc/resolv.conf
{
    echo "nameserver 1.1.1.1"
    echo "nameserver 8.8.8.8"
} > /etc/resolv.conf

echo "forkd-init: launching agent..."
# Find python: Ubuntu has /usr/bin/python3; official python:* images have /usr/local/bin/python3.
for PY in /usr/local/bin/python3 /usr/bin/python3 /usr/local/bin/python /usr/bin/python; do
    if [ -x "$PY" ]; then
        exec "$PY" /forkd-agent.py
    fi
done
echo "forkd-init: ERROR: no python interpreter found in /usr/bin or /usr/local/bin" >&2
# Park PID 1 so the kernel doesn't panic. The agent won't be available
# but at least snapshot/restore plumbing still works for debugging.
exec sleep infinity
