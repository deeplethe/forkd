#!/usr/bin/env bash
# probe-bpftrace-fc.sh — bpftrace-sample firecracker during a slow BRANCH
# to identify the user-space function the multi-BRANCH anomaly burns
# time in. Companion to PROBE-multi-branch-anomaly.md.
#
# Approach:
#   1. spawn a source sandbox
#   2. take N-1 BRANCHes to warm into the slow regime (BRANCH 6+ is
#      ~800-1500 ms vs ~250 ms for BRANCH 1-2 on coding-agent-fork-prewarm-v1)
#   3. start bpftrace sampling user-space stacks at 199 Hz on FC pid
#   4. fire the Nth BRANCH (slow); bpftrace captures stacks throughout
#   5. detach bpftrace, print top stacks by sample count
#
# Output: /tmp/fc-stacks-<unix>.txt with the sampled stacks.
# The hottest stack tells us what user-space code Firecracker is in
# when the BRANCH N >= 6 takes ~5x longer than BRANCH 1-2.
set -euo pipefail

FORKD_URL=${FORKD_URL:-http://127.0.0.1:8889}
FORKD_TOKEN=${FORKD_TOKEN:-$(cat "${FORKD_TOKEN_FILE:-/etc/forkd/token}" 2>/dev/null || echo "")}
TAG=${TAG:-coding-agent-fork-prewarm-v1}
WARMUP_BRANCHES=${WARMUP_BRANCHES:-6}
GAP_SECS=${GAP_SECS:-3}
OUT="/tmp/fc-bpftrace-$(date +%s).txt"

auth=(-H "Authorization: Bearer $FORKD_TOKEN")

echo "[probe] starting, will write to $OUT" >&2

# Spawn source
echo "[probe] spawn source from $TAG" >&2
spawn=$(curl -fsS "${auth[@]}" -H "Content-Type: application/json" \
  -d "{\"snapshot_tag\":\"$TAG\",\"n\":1,\"per_child_netns\":true}" \
  "$FORKD_URL/v1/sandboxes")
sb_id=$(echo "$spawn" | jq -r '.[0].id')
fc_pid=$(echo "$spawn" | jq -r '.[0].pid')
echo "[probe] sandbox=$sb_id fc_pid=$fc_pid" >&2
sleep 2

# Warmup: take WARMUP_BRANCHES-1 BRANCHes so the next one is in the slow regime
for i in $(seq 1 $((WARMUP_BRANCHES - 1))); do
  sleep "$GAP_SECS"
  btag="warmup-${i}-$(date +%s%N)"
  resp=$(curl -fsS "${auth[@]}" -H "Content-Type: application/json" \
    -d "{\"tag\":\"$btag\",\"diff\":true}" \
    "$FORKD_URL/v1/sandboxes/$sb_id/branch")
  pause_ms=$(echo "$resp" | jq -r '.pause_ms')
  echo "[probe] warmup BRANCH $i: pause_ms=$pause_ms" >&2
done

# Start bpftrace BEFORE the final BRANCH; sample user stacks at 199 Hz.
# Filter to just our FC pid. ustack(perf,128) tries hard to symbolize.
echo "[probe] starting bpftrace on pid $fc_pid (199 Hz, user stacks)" >&2
sudo bpftrace -e "
profile:hz:199 / pid == $fc_pid /
{
    @[ustack(perf, 32)] = count();
}
interval:s:30 { exit(); }
" > "$OUT" 2>&1 &
bp_pid=$!
sleep 0.5

# Fire the slow BRANCH
echo "[probe] firing final (slow) BRANCH" >&2
sleep "$GAP_SECS"
btag="profiled-$(date +%s%N)"
t0_ns=$(date +%s%N)
resp=$(curl -fsS "${auth[@]}" -H "Content-Type: application/json" \
  -d "{\"tag\":\"$btag\",\"diff\":true}" \
  "$FORKD_URL/v1/sandboxes/$sb_id/branch")
t1_ns=$(date +%s%N)
wall_ms=$(( (t1_ns - t0_ns) / 1000000 ))
pause_ms=$(echo "$resp" | jq -r '.pause_ms')
diff_ms=$(echo "$resp" | jq -r '.diff_ms')
echo "[probe] profiled BRANCH: wall=${wall_ms}ms pause_ms=${pause_ms} diff_ms=${diff_ms}" >&2

# Stop bpftrace
sudo kill -INT "$bp_pid" 2>/dev/null || true
wait "$bp_pid" 2>/dev/null || true

# Cleanup
curl -fsS -X DELETE "${auth[@]}" "$FORKD_URL/v1/sandboxes/$sb_id" > /dev/null || true

echo "" >&2
echo "[probe] done. Top stacks by sample count:" >&2
echo "[probe] output saved at $OUT" >&2

# Print top hottest stacks. bpftrace -e ... > out produces output in @{...}
# format. We grep + sort.
echo "" >&2
echo "==== top stacks ====" >&2
# Find lines with @[..] = N and sort by N desc.
awk '/^@\[/,/^\]: [0-9]+$/' "$OUT" | tail -100 >&2 || true
