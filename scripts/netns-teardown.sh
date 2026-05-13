#!/usr/bin/env bash
# netns-teardown.sh — reverse netns-setup.sh.
#
# Removes the per-child network namespaces created by netns-setup.sh.
# Optionally removes the host bridge and its iptables MASQUERADE rules.
#
# Safety
# ------
# By default this script runs as a DRY RUN: it lists what it would delete
# and exits without touching anything. Pass --yes to actually delete.
#
# It only matches resources by EXACT NAME PATTERN:
#   - netns:  ^forkd-child-[0-9]+$
#   - bridge: forkd-br0 (only with --include-bridge)
#   - tap:    forkd-tap0 (only with --include-tap; usually owned by host-tap.sh)
#
# It will NEVER touch docker0, br-<hex> docker bridges, or any other
# user-owned netns / veth / bridge.
#
# Per-veth note: deleting a netns automatically destroys the veth pair
# (the host-side forkd-v-Nh disappears along with veth0 inside the ns),
# so we don't enumerate veths separately.
#
# Usage
# -----
#   sudo bash scripts/netns-teardown.sh                  # dry run
#   sudo bash scripts/netns-teardown.sh --yes            # delete netns only
#   sudo bash scripts/netns-teardown.sh --yes --include-bridge
#   sudo bash scripts/netns-teardown.sh --yes --include-bridge --include-tap

set -euo pipefail

DRY_RUN=true
INCLUDE_BRIDGE=false
INCLUDE_TAP=false

BRIDGE="${BRIDGE:-forkd-br0}"
TAP="${TAP:-forkd-tap0}"

while [ $# -gt 0 ]; do
    case "$1" in
        -y|--yes)            DRY_RUN=false ;;
        --include-bridge)    INCLUDE_BRIDGE=true ;;
        --include-tap)       INCLUDE_TAP=true ;;
        -h|--help)
            sed -n '1,/^# ---/p; /^# Usage/,/^set -e/p' "$0" | sed -n '1,/^set -e/p' | sed 's/^# \?//' | head -40
            exit 0
            ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
    shift
done

[ "$(id -u)" -eq 0 ] || { echo "run as root (sudo bash $0 ...)" >&2; exit 1; }
command -v ip >/dev/null || { echo "ip(8) not found" >&2; exit 1; }

say() { printf "\033[1;34m==>\033[0m %s\n" "$*"; }

if [ "$DRY_RUN" = true ]; then
    say "DRY RUN — nothing will be deleted. Pass --yes to actually delete."
fi

# ----- enumerate forkd-child-* netns ------------------------------------
mapfile -t NETNS_LIST < <(
    ip netns list 2>/dev/null \
        | awk '{print $1}' \
        | grep -E '^forkd-child-[0-9]+$' \
        | sort -V
)

if [ "${#NETNS_LIST[@]}" -eq 0 ]; then
    say "no forkd-child-* netns to remove"
else
    say "found ${#NETNS_LIST[@]} netns to remove:"
    printf '   %s\n' "${NETNS_LIST[@]}"
fi

# ----- enumerate bridge + tap (only with explicit flags) ----------------
DELETE_BRIDGE=false
if [ "$INCLUDE_BRIDGE" = true ]; then
    if ip link show "$BRIDGE" >/dev/null 2>&1; then
        DELETE_BRIDGE=true
        say "will remove bridge: $BRIDGE"
    else
        say "bridge $BRIDGE absent — skipping"
    fi
fi

DELETE_TAP=false
if [ "$INCLUDE_TAP" = true ]; then
    if ip link show "$TAP" >/dev/null 2>&1; then
        DELETE_TAP=true
        say "will remove tap: $TAP  (note: usually owned by scripts/host-tap.sh)"
    else
        say "tap $TAP absent — skipping"
    fi
fi

if [ "$DRY_RUN" = true ]; then
    echo
    say "stop here — pass --yes to delete the items listed above."
    exit 0
fi

# ----- delete netns (also auto-cleans paired veth) ----------------------
for ns in "${NETNS_LIST[@]}"; do
    # Belt-and-suspenders: assert pattern again right before delete.
    case "$ns" in
        forkd-child-*) ;;
        *) echo "REFUSING to delete '$ns' — unexpected name" >&2; continue ;;
    esac
    if ip netns delete "$ns" 2>/dev/null; then
        echo "  - deleted netns $ns"
    else
        echo "  ! failed to delete netns $ns" >&2
    fi
done

# ----- optionally remove bridge + iptables NAT rules --------------------
if [ "$DELETE_BRIDGE" = true ]; then
    case "$BRIDGE" in
        forkd-br0) ;;
        *) echo "REFUSING to delete bridge '$BRIDGE' — unexpected name" >&2; exit 1 ;;
    esac
    # Remove NAT + FORWARD rules tied to 10.43.0.0/16 ↔ uplink (created
    # by netns-setup.sh). Find the uplink that owns the MASQUERADE rule.
    UPLINK_RULES=$(
        iptables -t nat -S POSTROUTING 2>/dev/null \
            | awk '/-s 10\.43\.0\.0\/16/ && /-j MASQUERADE/ { for(i=1;i<=NF;i++) if($i=="-o") print $(i+1) }' \
            | sort -u
    )
    for UP in $UPLINK_RULES; do
        iptables -t nat -D POSTROUTING -s 10.43.0.0/16 -o "$UP" -j MASQUERADE 2>/dev/null \
            && echo "  - deleted iptables NAT POSTROUTING ($UP)"
        iptables -D FORWARD -i "$BRIDGE" -o "$UP" -j ACCEPT 2>/dev/null \
            && echo "  - deleted iptables FORWARD -i $BRIDGE -o $UP"
        iptables -D FORWARD -i "$UP" -o "$BRIDGE" -m state --state RELATED,ESTABLISHED -j ACCEPT 2>/dev/null \
            && echo "  - deleted iptables FORWARD -i $UP -o $BRIDGE"
    done
    ip link delete "$BRIDGE" 2>/dev/null \
        && echo "  - deleted bridge $BRIDGE" \
        || echo "  ! failed to delete bridge $BRIDGE" >&2
fi

if [ "$DELETE_TAP" = true ]; then
    case "$TAP" in
        forkd-tap0) ;;
        *) echo "REFUSING to delete tap '$TAP' — unexpected name" >&2; exit 1 ;;
    esac
    ip link delete "$TAP" 2>/dev/null \
        && echo "  - deleted tap $TAP" \
        || echo "  ! failed to delete tap $TAP" >&2
fi

say "done."
