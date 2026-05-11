#!/usr/bin/env bash
# day1-boot.sh — manually boot a vanilla Firecracker microVM, capture serial output, shut down.
#
# This is the WEEK1.md Day 1 exit criterion: prove Firecracker runs on this host.
# No SSH, no networking — just a console boot to verify the kernel + rootfs combo
# comes up to a login prompt.
#
# Run from anywhere. Defaults assume ~/work/fc-quickstart has the kernel + rootfs.

set -euo pipefail

WORK="${WORK:-$HOME/work/fc-quickstart}"
KERNEL="${KERNEL:-$WORK/vmlinux-6.1.141}"
ROOTFS="${ROOTFS:-$WORK/ubuntu-24.04.squashfs}"
SOCK="${SOCK:-/tmp/forkd-day1.sock}"
CONSOLE="${CONSOLE:-/tmp/forkd-day1.console}"
LIFETIME="${LIFETIME:-15}"  # seconds to let the VM run

say()  { printf "\033[1;34m==>\033[0m %s\n" "$*"; }
die()  { printf "\033[1;31merror:\033[0m %s\n" "$*" >&2; exit 1; }

[ -f "$KERNEL" ] || die "kernel not found at $KERNEL"
[ -f "$ROOTFS" ] || die "rootfs not found at $ROOTFS"
command -v firecracker >/dev/null || die "firecracker not on PATH"
[ -w /dev/kvm ] || die "/dev/kvm not writable for current user (run via 'sg kvm -c \"$0\"' or re-login)"

rm -f "$SOCK" "$CONSOLE"
trap 'rm -f "$SOCK"; pkill -P $$ firecracker 2>/dev/null || true' EXIT

say "starting firecracker (sock=$SOCK, console=$CONSOLE)..."
firecracker --api-sock "$SOCK" </dev/null >"$CONSOLE" 2>&1 &
FC_PID=$!

# Wait for API socket to appear (up to 2s).
for _ in $(seq 1 20); do
    [ -S "$SOCK" ] && break
    sleep 0.1
done
[ -S "$SOCK" ] || die "firecracker API socket never appeared"

api() {
    local method="$1" path="$2" body="$3"
    curl -sS --unix-socket "$SOCK" -X "$method" "http://localhost$path" \
        -H 'Content-Type: application/json' \
        -d "$body" || die "API call failed: $method $path"
}

say "configuring boot source..."
api PUT /boot-source "$(cat <<EOF
{
  "kernel_image_path": "$KERNEL",
  "boot_args": "console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda ro"
}
EOF
)"

say "configuring rootfs drive..."
api PUT /drives/rootfs "$(cat <<EOF
{
  "drive_id": "rootfs",
  "path_on_host": "$ROOTFS",
  "is_root_device": true,
  "is_read_only": true
}
EOF
)"

say "machine config (2 vCPU, 1 GiB)..."
api PUT /machine-config '{"vcpu_count": 2, "mem_size_mib": 1024}'

say "starting microVM..."
api PUT /actions '{"action_type": "InstanceStart"}'

say "VM running. Letting it boot for ${LIFETIME}s..."
sleep "$LIFETIME"

say "sending CtrlAltDel for clean shutdown..."
api PUT /actions '{"action_type": "SendCtrlAltDel"}' || true
sleep 2

if kill -0 "$FC_PID" 2>/dev/null; then
    say "firecracker still running, killing..."
    kill "$FC_PID" 2>/dev/null || true
    sleep 1
    kill -9 "$FC_PID" 2>/dev/null || true
fi

say "DONE. Console output (last 40 lines):"
echo "----------------------------------------"
tail -40 "$CONSOLE"
echo "----------------------------------------"
say "full console saved to: $CONSOLE"
