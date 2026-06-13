#!/usr/bin/env bash
# netns-setup.sh — provision N per-child network namespaces (issue #1 fix)
# and optionally outbound internet egress via a host bridge (issue #8).
#
# Layout per child i:
#   netns forkd-child-i
#     ├─ lo                                       (loopback)
#     ├─ forkd-tap0       10.42.0.1/24           (faces guest VM)
#     └─ veth0            10.43.0.<i+1>/16       (faces host bridge)
#   host
#     ├─ forkd-br0        10.43.0.1/16           (bridge for outbound NAT)
#     └─ vethN_h ←→ vethN_c (veth pair into netns N)
#
# Inside each netns:
#   - default route via 10.43.0.1 (the bridge)
#   - SNAT from 10.42.0.0/24 → 10.43.0.<i+1>  (so packets carry a
#     unique source the host bridge can reverse-route)
#
# Host:
#   - MASQUERADE 10.43.0.0/16 outbound through the default uplink
#   - net.ipv4.ip_forward = 1 (host + each netns)
#
# Run as root. Idempotent (re-running is safe).
#
# Usage:
#   sudo bash scripts/netns-setup.sh <N> [user]
#
# Example:
#   sudo bash scripts/netns-setup.sh 10              # owns to $SUDO_USER
#   sudo bash scripts/netns-setup.sh 10 alice        # owns to a specific user

set -euo pipefail

N="${1:-10}"
# $USER is unset in non-login shells (docker exec, some CI) and this
# script runs under `set -u` — fall back through id(1).
USER_OWNS="${2:-${SUDO_USER:-${USER:-$(id -un)}}}"
HOST_IP="${HOST_IP:-10.42.0.1}"

say() { printf "\033[1;34m==>\033[0m %s\n" "$*"; }
die() { printf "\033[1;31merror:\033[0m %s\n" "$*" >&2; exit 1; }

[ "$(id -u)" -eq 0 ] || die "run as root (sudo bash $0 $N)"
command -v ip >/dev/null   || die "ip(8) not found"

UPLINK="${UPLINK:-$(ip route show default | awk '/default/ {print $5; exit}')}"
BRIDGE="${BRIDGE:-forkd-br0}"
BRIDGE_IP="${BRIDGE_IP:-10.43.0.1}"

say "provisioning $N per-child netns (tap owner: $USER_OWNS)"
say "host bridge: $BRIDGE @ $BRIDGE_IP/16, uplink: $UPLINK"

# ----- host bridge for outbound NAT --------------------------------------
if ! ip link show "$BRIDGE" >/dev/null 2>&1; then
    ip link add "$BRIDGE" type bridge
fi
ip addr flush dev "$BRIDGE" || true
ip addr add "$BRIDGE_IP/16" dev "$BRIDGE" 2>/dev/null || true
ip link set "$BRIDGE" up

# Enable IP forwarding on host
echo 1 > /proc/sys/net/ipv4/ip_forward

# MASQUERADE outbound from the child subnet through the host uplink
if [ -n "$UPLINK" ]; then
    if ! iptables -t nat -C POSTROUTING -s 10.43.0.0/16 -o "$UPLINK" -j MASQUERADE 2>/dev/null; then
        iptables -t nat -A POSTROUTING -s 10.43.0.0/16 -o "$UPLINK" -j MASQUERADE
    fi
    if ! iptables -C FORWARD -i "$BRIDGE" -o "$UPLINK" -j ACCEPT 2>/dev/null; then
        iptables -A FORWARD -i "$BRIDGE" -o "$UPLINK" -j ACCEPT
    fi
    if ! iptables -C FORWARD -i "$UPLINK" -o "$BRIDGE" -m state --state RELATED,ESTABLISHED -j ACCEPT 2>/dev/null; then
        iptables -A FORWARD -i "$UPLINK" -o "$BRIDGE" -m state --state RELATED,ESTABLISHED -j ACCEPT
    fi
fi

# ----- per-child setup ---------------------------------------------------
for i in $(seq 1 "$N"); do
    NS="forkd-child-$i"
    TAP="forkd-tap0"
    VETH_H="forkd-v-${i}h"   # host side
    VETH_C="veth0"            # child-side (inside netns)
    CHILD_IP="10.43.0.$((i + 1))"

    # Create netns if absent
    if ! ip netns list | grep -q "^$NS\b"; then
        ip netns add "$NS"
    fi

    ip netns exec "$NS" ip link set lo up

    # Tap (faces the guest VM)
    if ! ip netns exec "$NS" ip link show "$TAP" >/dev/null 2>&1; then
        ip netns exec "$NS" ip tuntap add "$TAP" mode tap user "$USER_OWNS"
    fi
    ip netns exec "$NS" ip addr flush dev "$TAP" || true
    ip netns exec "$NS" ip addr add "$HOST_IP/24" dev "$TAP"
    ip netns exec "$NS" ip link set "$TAP" up

    # Veth pair (faces the host bridge)
    if ! ip link show "$VETH_H" >/dev/null 2>&1; then
        ip link add "$VETH_H" type veth peer name "$VETH_C"
        ip link set "$VETH_C" netns "$NS"
    fi
    # host side
    ip link set "$VETH_H" master "$BRIDGE"
    ip link set "$VETH_H" up
    # child side
    ip netns exec "$NS" ip addr flush dev "$VETH_C" || true
    ip netns exec "$NS" ip addr add "$CHILD_IP/16" dev "$VETH_C"
    ip netns exec "$NS" ip link set "$VETH_C" up

    # Default route inside netns via the bridge
    ip netns exec "$NS" ip route replace default via "$BRIDGE_IP" dev "$VETH_C"
    ip netns exec "$NS" sysctl -q -w net.ipv4.ip_forward=1

    # SNAT inside the netns: guest packets (src 10.42.0.0/24) leave with
    # the child's bridge-side IP so the bridge can reverse-route the reply.
    if ! ip netns exec "$NS" iptables -t nat -C POSTROUTING -s 10.42.0.0/24 -o "$VETH_C" -j SNAT --to-source "$CHILD_IP" 2>/dev/null; then
        ip netns exec "$NS" iptables -t nat -A POSTROUTING -s 10.42.0.0/24 -o "$VETH_C" -j SNAT --to-source "$CHILD_IP"
    fi

    printf "  %s ready (tap=%s, veth=%s ↔ %s, child=%s)\n" "$NS" "$TAP" "$VETH_C" "$VETH_H" "$CHILD_IP"
done

say "done."
echo
echo "Try:"
echo "  ip netns list"
echo "  forkd fork --tag pyagent -n $N --per-child-netns"
echo "  forkd exec --child forkd-child-1 -- echo hi"
