#!/usr/bin/env bash
# day6-scale.sh — scale 1-snapshot fork to N children, measure where it bends.
#
# WEEK1.md Day 6: push N=2 (day3) to N=10, N=50, N=100 and see what breaks.
# We expect to hit some combination of: vsock CID conflicts, fd limits,
# host memory pressure, KSM lag, or process accounting weirdness.
#
# Usage:
#   day6-scale.sh [N]   # default 10
#
# Snapshot must already exist at $HOME/work/fc-quickstart/snap-day2/.

set -uo pipefail

N="${1:-10}"

WORK_FC="${WORK_FC:-$HOME/work/fc-quickstart}"
SNAP_DIR="${SNAP_DIR:-$WORK_FC/snap-day2}"
VMSTATE="$SNAP_DIR/vmstate"
MEMORY="$SNAP_DIR/memory.bin"

[ -f "$VMSTATE" ] || { echo "snapshot not found at $VMSTATE — run day2-snapshot.sh first" >&2; exit 1; }
[ -f "$MEMORY" ]  || { echo "memory.bin not found at $MEMORY — run day2-snapshot.sh first" >&2; exit 1; }
command -v firecracker >/dev/null || { echo "firecracker not on PATH" >&2; exit 1; }
[ -w /dev/kvm ] || { echo "/dev/kvm not writable; try: sg kvm -c \"$0 $N\"" >&2; exit 1; }

WORK="/tmp/forkd-day6"
mkdir -p "$WORK"
rm -f "$WORK"/child-*.sock "$WORK"/child-*.console

say() { printf "\033[1;34m==>\033[0m %s\n" "$*"; }
ok()  { printf "\033[1;32m  ✓\033[0m %s\n" "$*"; }
bad() { printf "\033[1;31m  ✗\033[0m %s\n" "$*"; }

declare -a PIDS

cleanup() {
    for pid in "${PIDS[@]:-}"; do
        [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
    done
}
trap cleanup EXIT

api() {
    local sock="$1" method="$2" path="$3" body="$4"
    curl -sS --max-time 5 --unix-socket "$sock" -X "$method" "http://localhost$path" \
        -H 'Content-Type: application/json' -d "$body"
}

# host mem in MiB
host_mem_used_mib() {
    awk '$1 ~ /Mem|内存/ {used=$3} END {print used}' < <(free -m)
}

# total file descriptors open by this user — count /proc/*/fd entries directly,
# much faster than 'lsof -u' which exhaustively scans every process on the host.
my_fds() {
    local uid total=0 owner n
    uid=$(id -u)
    for d in /proc/[0-9]*/fd; do
        [ -d "$d" ] || continue
        owner=$(stat -c %u "$d" 2>/dev/null) || continue
        [ "$owner" = "$uid" ] || continue
        n=$(ls -1U "$d" 2>/dev/null | wc -l)
        total=$((total + n))
    done
    echo "$total"
}

# ---------------------------------------------------------------------------
say "scaling target: N=$N children from 1 snapshot"
echo "    memory.bin: $(stat -c '%s' "$MEMORY") bytes ($(numfmt --to=iec --suffix=B "$(stat -c %s "$MEMORY")"))"
echo "    naive cost if no sharing: $((N * 512)) MiB"

# baseline
mem_before=$(host_mem_used_mib)
fds_before=$(my_fds)

# ---------------------------------------------------------------------------
say "spawning $N firecracker processes..."
t0=$(date +%s.%N)
for i in $(seq 1 "$N"); do
    sock="$WORK/child-$i.sock"
    console="$WORK/child-$i.console"
    firecracker --api-sock "$sock" </dev/null >"$console" 2>&1 &
    PIDS+=($!)
done

# wait for all sockets to appear
missing=0
for i in $(seq 1 "$N"); do
    sock="$WORK/child-$i.sock"
    for _ in $(seq 1 60); do
        [ -S "$sock" ] && break
        sleep 0.05
    done
    [ -S "$sock" ] || { missing=$((missing+1)); bad "socket child-$i.sock never appeared"; }
done
t_spawn=$(date +%s.%N)
spawn_ms=$(awk "BEGIN { printf \"%.0f\", ($t_spawn - $t0)*1000 }")
ok "all sockets up in ${spawn_ms} ms ($missing missing)"

# ---------------------------------------------------------------------------
say "parallel restore from same snapshot..."
t0=$(date +%s.%N)
restore_pids=()
for i in $(seq 1 "$N"); do
    sock="$WORK/child-$i.sock"
    {
        api "$sock" PUT /snapshot/load "$(cat <<EOF
{
  "snapshot_path": "$VMSTATE",
  "mem_backend": {"backend_path": "$MEMORY", "backend_type": "File"},
  "enable_diff_snapshots": false,
  "resume_vm": true
}
EOF
)" >/dev/null 2>&1 || bad "restore failed for child-$i"
    } &
    restore_pids+=($!)
done
# IMPORTANT: wait only for the curl subshells. Bare `wait` would also wait
# for the firecracker processes themselves (also background children of this
# shell), which never exit on their own — that hangs the script forever.
for pid in "${restore_pids[@]}"; do wait "$pid"; done
t1=$(date +%s.%N)
restore_ms=$(awk "BEGIN { printf \"%.0f\", ($t1 - $t0)*1000 }")
ok "$N restores fired in parallel in ${restore_ms} ms"

# settle
sleep 2

# ---------------------------------------------------------------------------
say "counting alive children..."
alive=0
dead_pids=""
for pid in "${PIDS[@]}"; do
    if kill -0 "$pid" 2>/dev/null; then
        alive=$((alive+1))
    else
        dead_pids="$dead_pids $pid"
    fi
done
ok "$alive / $N children alive"
[ -n "$dead_pids" ] && bad "dead pids:$dead_pids"

# ---------------------------------------------------------------------------
say "measuring memory cost..."
mem_after=$(host_mem_used_mib)
fds_after=$(my_fds)
mem_delta=$((mem_after - mem_before))
fds_delta=$((fds_after - fds_before))

# sum smaps Rss / Shared_Clean / Private_Dirty across all live children
sum_rss=0; sum_shared=0; sum_private_dirty=0
for pid in "${PIDS[@]}"; do
    [ -r "/proc/$pid/smaps" ] || continue
    while IFS=' ' read -r r s p; do
        sum_rss=$((sum_rss + r))
        sum_shared=$((sum_shared + s))
        sum_private_dirty=$((sum_private_dirty + p))
    done < <(awk -v target="$MEMORY" '
        /^[0-9a-f]+-[0-9a-f]+ / { in_t = (index($0, target) > 0); next }
        in_t && /^Rss:/             { rss            += $2 }
        in_t && /^Shared_Clean:/    { shared         += $2 }
        in_t && /^Private_Dirty:/   { priv_dirty     += $2 }
        END { print rss " " shared " " priv_dirty }
    ' "/proc/$pid/smaps")
done

# ---------------------------------------------------------------------------
echo
say "=== Day 6 results: N=$N ==="
printf "  spawn time         : %4d ms\n" "$spawn_ms"
printf "  restore time       : %4d ms (parallel)\n" "$restore_ms"
printf "  total wall-clock   : %4d ms\n" "$((spawn_ms + restore_ms))"
printf "  alive children     : %d / %d\n" "$alive" "$N"
echo
printf "  naive cost         : %4d MiB  (= %d × 512 MiB)\n" "$((N * 512))" "$N"
printf "  host mem delta     : %4d MiB\n" "$mem_delta"
printf "  Σ Rss(memory.bin)  : %4d MiB\n" "$((sum_rss / 1024))"
printf "  Σ Shared_Clean     : %4d MiB  ← shared via CoW across all children\n" "$((sum_shared / 1024))"
printf "  Σ Private_Dirty    : %4d MiB  ← divergence after fork\n" "$((sum_private_dirty / 1024))"
if [ "$mem_delta" -gt 0 ]; then
    printf "  compression ratio  : %.1fx  (naive / actual)\n" \
        "$(awk "BEGIN { printf \"%.1f\", ($N * 512) / $mem_delta }")"
fi
echo
printf "  fd delta           : %4d  (was %d → now %d)\n" "$fds_delta" "$fds_before" "$fds_after"

# ---------------------------------------------------------------------------
say "shutting down..."
for sock in "$WORK"/child-*.sock; do
    [ -S "$sock" ] || continue
    curl -sS --max-time 2 --unix-socket "$sock" -X PUT 'http://localhost/actions' \
        -H 'Content-Type: application/json' \
        -d '{"action_type": "SendCtrlAltDel"}' >/dev/null 2>&1 || true
done
sleep 3
for pid in "${PIDS[@]}"; do kill "$pid" 2>/dev/null; done
PIDS=()

echo
say "Day 6 (N=$N) complete."
