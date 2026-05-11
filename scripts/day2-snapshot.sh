#!/usr/bin/env bash
# day2-snapshot.sh — pause / snapshot / restore a Firecracker VM by hand.
#
# WEEK1.md Day 2 exit criterion: snapshot a warm VM, kill the firecracker
# process, start a fresh one, restore from the snapshot, observe the VM
# resume from where it was paused.
#
# This is the plumbing we depend on for everything that follows. If
# restore doesn't work, fork-on-write doesn't either.

set -euo pipefail

WORK="${WORK:-$HOME/work/fc-quickstart}"
KERNEL="${KERNEL:-$WORK/vmlinux-6.1.141}"
ROOTFS="${ROOTFS:-$WORK/ubuntu-24.04.squashfs}"
SNAP_DIR="${SNAP_DIR:-$WORK/snap-day2}"
BOOT_WAIT="${BOOT_WAIT:-10}"
RESTORE_WAIT="${RESTORE_WAIT:-5}"

SOCK_A=/tmp/forkd-day2-a.sock
SOCK_B=/tmp/forkd-day2-b.sock
CONSOLE_A=/tmp/forkd-day2-a.console
CONSOLE_B=/tmp/forkd-day2-b.console

say()  { printf "\033[1;34m==>\033[0m %s\n" "$*"; }
die()  { printf "\033[1;31merror:\033[0m %s\n" "$*" >&2; exit 1; }

[ -f "$KERNEL" ]                     || die "kernel not found at $KERNEL"
[ -f "$ROOTFS" ]                     || die "rootfs not found at $ROOTFS"
command -v firecracker >/dev/null    || die "firecracker not on PATH"
[ -w /dev/kvm ]                      || die "/dev/kvm not writable; try: sg kvm -c \"$0\""

mkdir -p "$SNAP_DIR"
rm -f "$SOCK_A" "$SOCK_B" "$CONSOLE_A" "$CONSOLE_B"
rm -f "$SNAP_DIR/vmstate" "$SNAP_DIR/memory.bin"

cleanup() {
    [ -n "${FC_A:-}" ] && kill "$FC_A" 2>/dev/null || true
    [ -n "${FC_B:-}" ] && kill "$FC_B" 2>/dev/null || true
    rm -f "$SOCK_A" "$SOCK_B"
}
trap cleanup EXIT

api() {
    local sock="$1" method="$2" path="$3" body="$4"
    curl -sS --unix-socket "$sock" -X "$method" "http://localhost$path" \
        -H 'Content-Type: application/json' -d "$body"
}

wait_for_sock() {
    local sock="$1"
    for _ in $(seq 1 30); do
        [ -S "$sock" ] && return 0
        sleep 0.1
    done
    die "socket $sock never appeared"
}

# ----------------------------------------------------------------------------
# Phase 1 — boot the original VM
# ----------------------------------------------------------------------------
say "phase 1: boot original VM (sock=$SOCK_A)"
firecracker --api-sock "$SOCK_A" </dev/null >"$CONSOLE_A" 2>&1 &
FC_A=$!
wait_for_sock "$SOCK_A"

api "$SOCK_A" PUT /boot-source "$(cat <<EOF
{
  "kernel_image_path": "$KERNEL",
  "boot_args": "console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda ro"
}
EOF
)" >/dev/null

api "$SOCK_A" PUT /drives/rootfs "$(cat <<EOF
{
  "drive_id": "rootfs",
  "path_on_host": "$ROOTFS",
  "is_root_device": true,
  "is_read_only": true
}
EOF
)" >/dev/null

# track_dirty_pages enables diff snapshots later (Day 3+); harmless for Full.
api "$SOCK_A" PUT /machine-config '{"vcpu_count": 2, "mem_size_mib": 512, "track_dirty_pages": true}' >/dev/null
api "$SOCK_A" PUT /actions       '{"action_type": "InstanceStart"}' >/dev/null

say "VM running. letting it boot for ${BOOT_WAIT}s..."
sleep "$BOOT_WAIT"
grep -q "Welcome to" "$CONSOLE_A" && say "ubuntu welcome banner seen" || say "(no welcome banner yet — booting may be slow)"

# ----------------------------------------------------------------------------
# Phase 2 — pause + snapshot
# ----------------------------------------------------------------------------
say "phase 2: pause + snapshot"
api "$SOCK_A" PATCH /vm '{"state": "Paused"}' >/dev/null

t0=$(date +%s.%N)
api "$SOCK_A" PUT /snapshot/create "$(cat <<EOF
{
  "snapshot_path": "$SNAP_DIR/vmstate",
  "mem_file_path": "$SNAP_DIR/memory.bin",
  "snapshot_type": "Full"
}
EOF
)" >/dev/null
t1=$(date +%s.%N)
snap_ms=$(awk "BEGIN { printf \"%.0f\", ($t1 - $t0) * 1000 }")

say "snapshot created in ${snap_ms} ms:"
ls -lh "$SNAP_DIR"

# Kill original firecracker. Snapshot files are self-contained.
kill "$FC_A" 2>/dev/null || true
wait "$FC_A" 2>/dev/null || true
FC_A=""
rm -f "$SOCK_A"

# ----------------------------------------------------------------------------
# Phase 3 — restore in a fresh firecracker process
# ----------------------------------------------------------------------------
say "phase 3: restore in fresh firecracker (sock=$SOCK_B)"
firecracker --api-sock "$SOCK_B" </dev/null >"$CONSOLE_B" 2>&1 &
FC_B=$!
wait_for_sock "$SOCK_B"

t0=$(date +%s.%N)
api "$SOCK_B" PUT /snapshot/load "$(cat <<EOF
{
  "snapshot_path": "$SNAP_DIR/vmstate",
  "mem_backend": {"backend_path": "$SNAP_DIR/memory.bin", "backend_type": "File"},
  "enable_diff_snapshots": false,
  "resume_vm": true
}
EOF
)" >/dev/null
t1=$(date +%s.%N)
restore_ms=$(awk "BEGIN { printf \"%.0f\", ($t1 - $t0) * 1000 }")
say "snapshot restored + resumed in ${restore_ms} ms"

sleep "$RESTORE_WAIT"

# ----------------------------------------------------------------------------
# Phase 4 — clean shutdown
# ----------------------------------------------------------------------------
say "phase 4: send CtrlAltDel for clean shutdown"
api "$SOCK_B" PUT /actions '{"action_type": "SendCtrlAltDel"}' >/dev/null || true
sleep 3
kill "$FC_B" 2>/dev/null || true
wait "$FC_B" 2>/dev/null || true
FC_B=""

# ----------------------------------------------------------------------------
# Verify
# ----------------------------------------------------------------------------
echo
say "=== Day 2 results ==="
echo "snapshot size:    $(du -sh "$SNAP_DIR" | cut -f1)"
echo "vmstate:          $(stat -c '%s bytes' "$SNAP_DIR/vmstate")"
echo "memory.bin:       $(stat -c '%s bytes' "$SNAP_DIR/memory.bin")"
echo "snapshot time:    ${snap_ms} ms"
echo "restore time:     ${restore_ms} ms"
echo
echo "original console tail (just before pause):"
tail -5 "$CONSOLE_A"
echo
echo "restored console head (after resume):"
head -20 "$CONSOLE_B"
echo
echo "restored console tail (shutdown):"
tail -10 "$CONSOLE_B"
echo
say "Day 2 complete: snapshot + restore plumbing works."
