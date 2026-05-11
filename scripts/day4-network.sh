#!/usr/bin/env bash
# day4-network.sh — host-side setup for parent VM networking.
#
# Creates a tap device that the Firecracker parent attaches to, assigns
# the host side an IP on a /24 subnet, and turns on IP forwarding + masquerade
# so the guest can reach the wider network through the host.
#
# Run once per host boot; idempotent.

set -euo pipefail

TAP="${TAP:-forkd-tap0}"
HOST_IP="${HOST_IP:-10.42.0.1}"
GUEST_NET="${GUEST_NET:-10.42.0.0/24}"
USER_OWNS="${USER_OWNS:-${SUDO_USER:-$USER}}"
UPLINK="${UPLINK:-}"   # auto-detect default route iface if empty

say() { printf "\033[1;34m==>\033[0m %s\n" "$*"; }
die() { printf "\033[1;31merror:\033[0m %s\n" "$*" >&2; exit 1; }

[ "$(id -u)" -eq 0 ] || die "run as root (sudo bash $0)"

if [ -z "$UPLINK" ]; then
    UPLINK="$(ip route show default | awk '/default/ {print $5; exit}')"
fi

say "tap         : $TAP (owned by $USER_OWNS)"
say "host IP     : $HOST_IP"
say "guest subnet: $GUEST_NET"
say "uplink      : ${UPLINK:-<none>}"

# ---- tap device ------------------------------------------------------------
if ip link show "$TAP" >/dev/null 2>&1; then
    say "$TAP already exists; reusing"
else
    ip tuntap add "$TAP" mode tap user "$USER_OWNS"
fi
ip addr flush dev "$TAP" || true
ip addr add "$HOST_IP/24" dev "$TAP"
ip link set "$TAP" up

# ---- forwarding + masquerade ----------------------------------------------
echo 1 > /proc/sys/net/ipv4/ip_forward

if [ -n "$UPLINK" ]; then
    # Idempotent: only add the rule if it doesn't already exist.
    if ! iptables -t nat -C POSTROUTING -s "$GUEST_NET" -o "$UPLINK" -j MASQUERADE 2>/dev/null; then
        iptables -t nat -A POSTROUTING -s "$GUEST_NET" -o "$UPLINK" -j MASQUERADE
    fi
    if ! iptables -C FORWARD -i "$TAP" -o "$UPLINK" -j ACCEPT 2>/dev/null; then
        iptables -A FORWARD -i "$TAP" -o "$UPLINK" -j ACCEPT
    fi
    if ! iptables -C FORWARD -i "$UPLINK" -o "$TAP" -m state --state RELATED,ESTABLISHED -j ACCEPT 2>/dev/null; then
        iptables -A FORWARD -i "$UPLINK" -o "$TAP" -m state --state RELATED,ESTABLISHED -j ACCEPT
    fi
fi

say "done."
echo
echo "Try:"
echo "  ip addr show $TAP"
echo "  forkd snapshot --tag netdemo --tap $TAP ..."
