#!/usr/bin/env bash
# day3-fork.sh — restore TWO children from ONE snapshot, verify CoW memory sharing.
#
# WEEK1.md Day 3 exit criterion: two firecracker processes, both restored
# from the same memory.bin via mmap(MAP_PRIVATE). /proc/PID/smaps should
# show Shared_Clean ≈ Rss for the memory.bin mapping — proving the kernel
# is letting both children share pages and copy-on-write on divergence.
#
# This is the conceptual core of forkd. If this works, the rest is engineering.

set -euo pipefail

WORK="${WORK:-$HOME/work/fc-quickstart}"
SNAP_DIR="${SNAP_DIR:-$WORK/snap-day2}"

VMSTATE="$SNAP_DIR/vmstate"
MEMORY="$SNAP_DIR/memory.bin"

[ -f "$VMSTATE" ] || { echo "snapshot not found at $VMSTATE — run day2-snapshot.sh first" >&2; exit 1; }
[ -f "$MEMORY" ]  || { echo "memory.bin not found at $MEMORY — run day2-snapshot.sh first" >&2; exit 1; }
command -v firecracker >/dev/null || { echo "firecracker not on PATH" >&2; exit 1; }
[ -w /dev/kvm ] || { echo "/dev/kvm not writable; try: sg kvm -c \"$0\"" >&2; exit 1; }

SOCK_1=/tmp/forkd-day3-child1.sock
SOCK_2=/tmp/forkd-day3-child2.sock
CONSOLE_1=/tmp/forkd-day3-child1.console
CONSOLE_2=/tmp/forkd-day3-child2.console
rm -f "$SOCK_1" "$SOCK_2" "$CONSOLE_1" "$CONSOLE_2"

say()  { printf "\033[1;34m==>\033[0m %s\n" "$*"; }
ok()   { printf "\033[1;32m  ✓\033[0m %s\n" "$*"; }
bad()  { printf "\033[1;31m  ✗\033[0m %s\n" "$*"; }

cleanup() {
    [ -n "${PID_1:-}" ] && kill "$PID_1" 2>/dev/null || true
    [ -n "${PID_2:-}" ] && kill "$PID_2" 2>/dev/null || true
    rm -f "$SOCK_1" "$SOCK_2"
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
    echo "socket $sock never appeared" >&2; exit 1
}

# Spawn child N with given sock+console
spawn_child() {
    local sock="$1" console="$2"
    firecracker --api-sock "$sock" </dev/null >"$console" 2>&1 &
    echo $!
}

restore_child() {
    local sock="$1"
    api "$sock" PUT /snapshot/load "$(cat <<EOF
{
  "snapshot_path": "$VMSTATE",
  "mem_backend": {"backend_path": "$MEMORY", "backend_type": "File"},
  "enable_diff_snapshots": false,
  "resume_vm": true
}
EOF
)" >/dev/null
}

# ---------------------------------------------------------------------------
say "starting 2 firecracker processes, restoring same snapshot..."

t0=$(date +%s.%N)
PID_1=$(spawn_child "$SOCK_1" "$CONSOLE_1")
PID_2=$(spawn_child "$SOCK_2" "$CONSOLE_2")
wait_for_sock "$SOCK_1"
wait_for_sock "$SOCK_2"

restore_child "$SOCK_1" &
restore_child "$SOCK_2" &
wait
t1=$(date +%s.%N)

ms=$(awk "BEGIN { printf \"%.0f\", ($t1 - $t0) * 1000 }")
ok "both children restored + resumed in ${ms} ms (parallel)"
echo "    child 1: pid=$PID_1 sock=$SOCK_1"
echo "    child 2: pid=$PID_2 sock=$SOCK_2"

# Let both run a moment so they touch memory pages.
sleep 2

# ---------------------------------------------------------------------------
say "verifying both children are alive..."
if kill -0 "$PID_1" 2>/dev/null; then ok "child 1 ($PID_1) alive"; else bad "child 1 ($PID_1) dead"; fi
if kill -0 "$PID_2" 2>/dev/null; then ok "child 2 ($PID_2) alive"; else bad "child 2 ($PID_2) dead"; fi

# ---------------------------------------------------------------------------
say "inspecting /proc/<pid>/smaps for memory.bin mapping..."

# Find the memory.bin VMA in each process and sum up sharing metrics.
analyze() {
    local pid="$1" label="$2"
    local maps="/proc/$pid/smaps"
    [ -r "$maps" ] || { bad "$label: cannot read $maps"; return; }

    # Find the line range describing memory.bin; sum Rss / Pss / Shared_Clean / Private_Dirty
    awk -v target="$MEMORY" '
        # New VMA header line
        /^[0-9a-f]+-[0-9a-f]+ / { in_target = (index($0, target) > 0); next }
        in_target && /^Rss:/             { rss             += $2 }
        in_target && /^Pss:/             { pss             += $2 }
        in_target && /^Shared_Clean:/    { shared_clean    += $2 }
        in_target && /^Shared_Dirty:/    { shared_dirty    += $2 }
        in_target && /^Private_Clean:/   { private_clean   += $2 }
        in_target && /^Private_Dirty:/   { private_dirty   += $2 }
        END {
            printf "  Rss          : %d kB\n", rss
            printf "  Pss          : %d kB   (≈ rss/N_sharers if CoW shared)\n", pss
            printf "  Shared_Clean : %d kB   ← if ≈ Rss, CoW working\n", shared_clean
            printf "  Shared_Dirty : %d kB\n", shared_dirty
            printf "  Private_Clean: %d kB\n", private_clean
            printf "  Private_Dirty: %d kB   ← writes that diverged after fork\n", private_dirty
        }
    ' "$maps"
}

echo
echo "child 1 ($PID_1):"
analyze "$PID_1" "child 1"
echo
echo "child 2 ($PID_2):"
analyze "$PID_2" "child 2"

# ---------------------------------------------------------------------------
say "host-level memory accounting..."
free -m | head -2
echo
echo "(if CoW is working, 2 children should NOT cost 2 × 512 MiB)"

# ---------------------------------------------------------------------------
say "shutting down both children..."
api "$SOCK_1" PUT /actions '{"action_type": "SendCtrlAltDel"}' >/dev/null || true
api "$SOCK_2" PUT /actions '{"action_type": "SendCtrlAltDel"}' >/dev/null || true
sleep 3
kill "$PID_1" "$PID_2" 2>/dev/null || true
wait "$PID_1" "$PID_2" 2>/dev/null || true
PID_1="" ; PID_2=""

echo
say "Day 3 complete."
