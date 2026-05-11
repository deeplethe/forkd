#!/usr/bin/env bash
# netns-setup.sh — provision N per-child network namespaces.
#
# Each child gets its own netns containing a `forkd-tap` device with
# host-side IP 10.42.0.1/24. Multiple children can share MAC + guest IP
# because each lives in its own namespace.
#
# Run as root. Idempotent (re-running is safe).
#
# Usage:
#   sudo bash scripts/netns-setup.sh <N> [user]
#
# Example:
#   sudo bash scripts/netns-setup.sh 10 yangdongxu

set -euo pipefail

N="${1:-10}"
USER_OWNS="${2:-${SUDO_USER:-$USER}}"
HOST_IP="${HOST_IP:-10.42.0.1}"

say() { printf "\033[1;34m==>\033[0m %s\n" "$*"; }
die() { printf "\033[1;31merror:\033[0m %s\n" "$*" >&2; exit 1; }

[ "$(id -u)" -eq 0 ] || die "run as root (sudo bash $0 $N)"
command -v ip >/dev/null   || die "ip(8) not found"

say "provisioning $N per-child netns (tap owner: $USER_OWNS)"

for i in $(seq 1 "$N"); do
    NS="forkd-child-$i"
    TAP="forkd-tap0"

    # Create netns if absent
    if ! ip netns list | grep -q "^$NS\b"; then
        ip netns add "$NS"
    fi

    # Set up loopback inside the netns
    ip netns exec "$NS" ip link set lo up

    # Create tap inside the netns
    if ! ip netns exec "$NS" ip link show "$TAP" >/dev/null 2>&1; then
        ip netns exec "$NS" ip tuntap add "$TAP" mode tap user "$USER_OWNS"
    fi

    ip netns exec "$NS" ip addr flush dev "$TAP" || true
    ip netns exec "$NS" ip addr add "$HOST_IP/24" dev "$TAP"
    ip netns exec "$NS" ip link set "$TAP" up

    printf "  %s ready (tap=%s, host=%s)\n" "$NS" "$TAP" "$HOST_IP"
done

say "done."
echo
echo "Try:"
echo "  ip netns list"
echo "  forkd fork --tag pyagent -n $N --per-child-netns"
echo "  forkd exec --child forkd-child-1 -- echo hi"
