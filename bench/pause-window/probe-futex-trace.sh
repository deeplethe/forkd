#!/usr/bin/env bash
# probe-futex-trace.sh — bpftrace the firecracker process's futex calls
# during a slow BRANCH to identify which specific futex(es) accumulate
# wait time.
#
# Follows up on PROBE-multi-branch-anomaly.md: thread-level probe found
# 17/250 in-kernel-sleep samples in futex_wait_queue (the only signal
# besides the parked vCPU). This script aggregates per-uaddr wait
# duration so we can:
#   1. Confirm that futex contention scales with snapshot count
#   2. Identify the specific futex (memory address; later cross-referenced
#      to a symbol/data structure via /proc/$pid/maps)
#
# Output: histogram of (uaddr, op) → total wait nanoseconds.
set -euo pipefail

FORKD_URL=${FORKD_URL:-http://127.0.0.1:8889}
FORKD_TOKEN=${FORKD_TOKEN:-$(cat "${FORKD_TOKEN_FILE:-/etc/forkd/token}" 2>/dev/null || echo "")}
TAG=${TAG:-coding-agent-fork-prewarm-v1}
WARMUP_BRANCHES=${WARMUP_BRANCHES:-6}
GAP_SECS=${GAP_SECS:-3}
OUT="/tmp/futex-trace-$(date +%s).txt"
auth=(-H "Authorization: Bearer $FORKD_TOKEN")

echo "[probe] output → $OUT" >&2

# Spawn source
spawn=$(curl -fsS "${auth[@]}" -H "Content-Type: application/json" \
  -d "{\"snapshot_tag\":\"$TAG\",\"n\":1,\"per_child_netns\":true}" \
  "$FORKD_URL/v1/sandboxes")
sb_id=$(echo "$spawn" | jq -r '.[0].id')
fc_pid=$(echo "$spawn" | jq -r '.[0].pid')
echo "[probe] sandbox=$sb_id fc_pid=$fc_pid" >&2
sleep 2

# Warmup into the slow regime
for i in $(seq 1 "$WARMUP_BRANCHES"); do
  sleep "$GAP_SECS"
  resp=$(curl -fsS "${auth[@]}" -H "Content-Type: application/json" \
    -d "{\"tag\":\"w-$i-$(date +%s%N)\",\"diff\":true}" \
    "$FORKD_URL/v1/sandboxes/$sb_id/branch")
  echo "[probe] warmup BRANCH $i: pause_ms=$(echo "$resp" | jq -r .pause_ms)" >&2
done

# Launch bpftrace — capture entry/exit of futex(2) and aggregate wait time
# per (uaddr, op) tuple.
# Note: `args` field in tracepoint:syscalls:sys_enter_futex has uaddr, op.
# FUTEX_WAIT_PRIVATE = 128 (most common), FUTEX_WAKE_PRIVATE = 129.
echo "[probe] starting bpftrace (8 s window covers the next BRANCH)" >&2
sudo bpftrace -e "
tracepoint:syscalls:sys_enter_futex
/ pid == $fc_pid /
{
    @start[tid] = nsecs;
    @uaddr[tid] = args->uaddr;
    @op[tid] = args->op;
}

tracepoint:syscalls:sys_exit_futex
/ pid == $fc_pid && @start[tid] != 0 /
{
    \$d = nsecs - @start[tid];
    @wait_ns[@uaddr[tid], @op[tid]] = sum(\$d);
    @wait_count[@uaddr[tid], @op[tid]] = count();
    delete(@start[tid]);
    delete(@uaddr[tid]);
    delete(@op[tid]);
}

interval:s:8 { exit(); }
" > "$OUT" 2>&1 &
bp_pid=$!
sleep 0.5

# Fire the slow BRANCH while bpftrace is recording
sleep "$GAP_SECS"
echo "[probe] firing profiled BRANCH" >&2
t0_ns=$(date +%s%N)
resp=$(curl -fsS "${auth[@]}" -H "Content-Type: application/json" \
  -d "{\"tag\":\"prof-$(date +%s%N)\",\"diff\":true}" \
  "$FORKD_URL/v1/sandboxes/$sb_id/branch")
t1_ns=$(date +%s%N)
echo "[probe] profiled BRANCH: pause_ms=$(echo "$resp" | jq -r .pause_ms) wall=$(( (t1_ns - t0_ns) / 1000000 ))ms" >&2

wait "$bp_pid" 2>/dev/null || true

# Save FC's /proc/$pid/maps so we can later annotate which mapping the
# top uaddr falls in.
sudo cp /proc/$fc_pid/maps "$OUT.maps" 2>/dev/null || true

# Cleanup
curl -fsS -X DELETE "${auth[@]}" "$FORKD_URL/v1/sandboxes/$sb_id" > /dev/null || true

echo "" >&2
echo "===== top futexes by total wait time =====" >&2
# bpftrace prints @wait_ns[u64, u64]: N — sort by N.
awk '
/^@wait_ns\[/ {
    # line looks like "@wait_ns[0x7fab12345678, 128]: 1234567890"
    match($0, /\[([0-9]+|0x[0-9a-fA-F]+), ([0-9]+)\]:\s*([0-9]+)/, m)
    if (m[3] > 0) {
        printf "  %s op=%s wait_ms=%.2f\n", m[1], m[2], m[3]/1e6
    }
}
' "$OUT" | sort -k4 -t= -n -r | head -10 >&2

echo "" >&2
echo "===== top futexes by call count =====" >&2
awk '
/^@wait_count\[/ {
    match($0, /\[([0-9]+|0x[0-9a-fA-F]+), ([0-9]+)\]:\s*([0-9]+)/, m)
    if (m[3] > 0) {
        printf "  %s op=%s calls=%s\n", m[1], m[2], m[3]
    }
}
' "$OUT" | sort -k4 -t= -n -r | head -10 >&2

echo "" >&2
echo "Raw trace: $OUT" >&2
echo "FC memory map: $OUT.maps" >&2
