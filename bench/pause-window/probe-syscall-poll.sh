#!/usr/bin/env bash
# probe-syscall-poll.sh — poll /proc/$pid/syscall during a slow BRANCH to
# see which syscall (if any) firecracker is blocked in when wall time
# explodes. Companion to PROBE-multi-branch-anomaly.md.
#
# The bpftrace probe (probe-bpftrace-fc.sh) showed FC was off-CPU ~95%
# of the slow BRANCH, contradicting the "user-space CPU" interpretation
# in PROBE-multi-branch-anomaly.md. This script polls /proc/<pid>/syscall
# at 200 Hz to see *which* syscall FC is sleeping in.
#
# Output: /tmp/syscall-poll-<unix>.txt — one line per sample:
#   <unix_ns>  <syscall_nr>  <arg0> <arg1> <arg2> ... <sp> <pc>
# or "running" if FC was on-CPU at that moment.
set -euo pipefail

FORKD_URL=${FORKD_URL:-http://127.0.0.1:8889}
FORKD_TOKEN=${FORKD_TOKEN:-$(cat "${FORKD_TOKEN_FILE:-/etc/forkd/token}" 2>/dev/null || echo "")}
TAG=${TAG:-coding-agent-fork-prewarm-v1}
WARMUP_BRANCHES=${WARMUP_BRANCHES:-5}
GAP_SECS=${GAP_SECS:-3}
OUT="/tmp/syscall-poll-$(date +%s).txt"
auth=(-H "Authorization: Bearer $FORKD_TOKEN")

echo "[probe] output → $OUT" >&2

spawn=$(curl -fsS "${auth[@]}" -H "Content-Type: application/json" \
  -d "{\"snapshot_tag\":\"$TAG\",\"n\":1,\"per_child_netns\":true}" \
  "$FORKD_URL/v1/sandboxes")
sb_id=$(echo "$spawn" | jq -r '.[0].id')
fc_pid=$(echo "$spawn" | jq -r '.[0].pid')
echo "[probe] sandbox=$sb_id fc_pid=$fc_pid" >&2
sleep 2

# Warmup
for i in $(seq 1 "$WARMUP_BRANCHES"); do
  sleep "$GAP_SECS"
  btag="warmup-${i}-$(date +%s%N)"
  resp=$(curl -fsS "${auth[@]}" -H "Content-Type: application/json" \
    -d "{\"tag\":\"$btag\",\"diff\":true}" \
    "$FORKD_URL/v1/sandboxes/$sb_id/branch")
  echo "[probe] warmup BRANCH $i: pause_ms=$(echo "$resp" | jq -r .pause_ms)" >&2
done

# Poll syscall in background. ~200 Hz (sleep 0.005).
sleep 1
(
    end_ns=$(( $(date +%s%N) + 3000000000 ))   # poll for 3s max
    while [ "$(date +%s%N)" -lt "$end_ns" ]; do
        ts=$(date +%s%N)
        sc=$(sudo cat /proc/$fc_pid/syscall 2>/dev/null || echo "gone")
        printf "%s  %s\n" "$ts" "$sc"
        sleep 0.005
    done
) > "$OUT" &
poll_pid=$!

# Fire the slow BRANCH while polling runs
sleep "$GAP_SECS"
echo "[probe] firing profiled BRANCH" >&2
t0_ns=$(date +%s%N)
resp=$(curl -fsS "${auth[@]}" -H "Content-Type: application/json" \
  -d "{\"tag\":\"profiled-$(date +%s%N)\",\"diff\":true}" \
  "$FORKD_URL/v1/sandboxes/$sb_id/branch")
t1_ns=$(date +%s%N)
echo "[probe] BRANCH pause_ms=$(echo "$resp" | jq -r .pause_ms) wall=$(( (t1_ns - t0_ns) / 1000000 ))ms" >&2

# Wait for poller (3s window) and clean up
wait "$poll_pid" 2>/dev/null || true
curl -fsS -X DELETE "${auth[@]}" "$FORKD_URL/v1/sandboxes/$sb_id" > /dev/null || true

# Print histogram of syscall_nr across the samples
echo "" >&2
echo "===== syscall_nr histogram (during the slow window) =====" >&2
awk -v t0="$t0_ns" -v t1="$t1_ns" '
$1 >= t0 && $1 <= t1 {
    # /proc/pid/syscall format: "<nr> <arg0> ... <sp> <pc>" or "running"
    if ($2 == "running") {
        nr = "running"
    } else {
        nr = $2
    }
    counts[nr]++
    total++
}
END {
    if (total == 0) {
        print "no samples during the slow window"
    } else {
        for (k in counts) {
            printf "  %-12s  %5d  %.1f%%\n", k, counts[k], counts[k]*100.0/total
        }
        printf "  %-12s  %5d  total samples\n", "(all)", total
    }
}' "$OUT" | sort -k2 -n -r >&2

echo "" >&2
echo "Raw poll log: $OUT" >&2
echo "Syscall numbers on x86_64 (key ones for KVM):" >&2
echo "  16 = ioctl  (KVM ioctls land here)" >&2
echo "  35 = nanosleep" >&2
echo "  17 = pread64" >&2
echo "  18 = pwrite64" >&2
echo "  74 = fsync" >&2
echo "  202 = futex" >&2
