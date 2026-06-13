#!/usr/bin/env bash
# host-tap.sh — provision the host-side tap for the parent VM.
#
# This is the simplest possible setup: one tap (forkd-tap0) on the host
# at 10.42.0.1/24, used by `forkd snapshot` to give the parent guest a
# working eth0 during warm-up.
#
# For multi-child fork-out, also run `scripts/netns-setup.sh N`, which
# provisions per-child network namespaces on top of a host bridge with
# its own MASQUERADE rules.
#
# Run as root. Idempotent.
set -euo pipefail

TAP="${TAP:-forkd-tap0}"
TAP_IP="${TAP_IP:-10.42.0.1}"
# $USER is unset in non-login shells (docker exec, some CI) and this
# script runs under `set -u` — fall back through id(1).
USER_OWNS="${USER_OWNS:-${SUDO_USER:-${USER:-$(id -un)}}}"

[ "$(id -u)" -eq 0 ] || { echo "run as root" >&2; exit 1; }
command -v ip >/dev/null || { echo "ip(8) required" >&2; exit 1; }

if ! ip link show "$TAP" >/dev/null 2>&1; then
    ip tuntap add "$TAP" mode tap user "$USER_OWNS"
fi
ip addr flush dev "$TAP" || true
ip addr add "$TAP_IP/24" dev "$TAP" 2>/dev/null || true
ip link set "$TAP" up

# Enable forwarding so MASQUERADE'd egress works.
echo 1 > /proc/sys/net/ipv4/ip_forward

UPLINK="${UPLINK:-$(ip route show default | awk '/default/ {print $5; exit}')}"
if [ -n "$UPLINK" ]; then
    if ! iptables -t nat -C POSTROUTING -s 10.42.0.0/24 -o "$UPLINK" -j MASQUERADE 2>/dev/null; then
        iptables -t nat -A POSTROUTING -s 10.42.0.0/24 -o "$UPLINK" -j MASQUERADE
    fi
    if ! iptables -C FORWARD -i "$TAP" -o "$UPLINK" -j ACCEPT 2>/dev/null; then
        iptables -A FORWARD -i "$TAP" -o "$UPLINK" -j ACCEPT
    fi
    if ! iptables -C FORWARD -i "$UPLINK" -o "$TAP" -m state --state RELATED,ESTABLISHED -j ACCEPT 2>/dev/null; then
        iptables -A FORWARD -i "$UPLINK" -o "$TAP" -m state --state RELATED,ESTABLISHED -j ACCEPT
    fi
fi

echo "tap $TAP ready at $TAP_IP/24 (owner: $USER_OWNS, uplink: ${UPLINK:-none})"
