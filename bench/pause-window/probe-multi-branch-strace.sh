#!/usr/bin/env bash
# probe-multi-branch-strace.sh — strace firecracker through N consecutive
# diff BRANCHes to see what syscall growth accounts for the BRANCH 3+
# pause-time anomaly documented in RESULTS-v0.3.md.
#
# Output: one strace-counts file per BRANCH (per-syscall totals between
# BRANCH-start and BRANCH-done as observed by the curl client).
#
# Usage:
#   sudo bash bench/pause-window/probe-multi-branch-strace.sh
#
# Reads from env:
#   FORKD_URL, FORKD_TOKEN_FILE, TAG, N_BRANCHES, GAP_SECS
#
# Output dir: $OUT_DIR (default /tmp/multi-branch-probe-<unix>)
set -euo pipefail

FORKD_URL=${FORKD_URL:-http://127.0.0.1:8889}
FORKD_TOKEN=${FORKD_TOKEN:-$(cat "${FORKD_TOKEN_FILE:-/etc/forkd/token}" 2>/dev/null || echo "")}
TAG=${TAG:-coding-agent-fork-prewarm-v1}
N_BRANCHES=${N_BRANCHES:-10}
GAP_SECS=${GAP_SECS:-3}
OUT_DIR=${OUT_DIR:-/tmp/multi-branch-probe-$(date +%s)}
mkdir -p "$OUT_DIR"

auth=(-H "Authorization: Bearer $FORKD_TOKEN")

echo "[probe] out_dir=$OUT_DIR tag=$TAG n=$N_BRANCHES" >&2

# Spawn the source sandbox; capture FC pid via the daemon.
echo "[probe] spawning source..." >&2
spawn=$(curl -fsS "${auth[@]}" -H "Content-Type: application/json" \
  -d "{\"snapshot_tag\":\"$TAG\",\"n\":1,\"per_child_netns\":true}" \
  "$FORKD_URL/v1/sandboxes")
sb_id=$(echo "$spawn" | jq -r '.[0].id')
fc_pid=$(echo "$spawn" | jq -r '.[0].pid')
echo "[probe] sandbox=$sb_id fc_pid=$fc_pid" >&2
sleep 2

# Pre-flight check: confirm FC pid is alive
if ! sudo kill -0 "$fc_pid" 2>/dev/null; then
  echo "[probe] FC pid $fc_pid not alive; aborting" >&2
  exit 1
fi

# Strace summary per BRANCH: start strace -c in background, do the BRANCH,
# detach (SIGINT), capture the per-syscall counts.
echo "branch_idx,pause_ms,diff_ms,diff_physical_bytes,strace_total_us,strace_calls" > "$OUT_DIR/summary.csv"

for i in $(seq 1 "$N_BRANCHES"); do
  sleep "$GAP_SECS"
  strace_log="$OUT_DIR/branch-$i.strace"

  # Attach strace with -c (summary mode) to the FC pid. Run async so the
  # BRANCH call can proceed; detach with SIGINT once the BRANCH returns.
  sudo strace -c -p "$fc_pid" -o "$strace_log" 2>/dev/null &
  strace_pid=$!
  # Give strace a moment to attach
  sleep 0.2

  btag="probe-${i}-$(date +%s%N)"
  resp=$(curl -fsS "${auth[@]}" -H "Content-Type: application/json" \
    -d "{\"tag\":\"$btag\",\"diff\":true}" \
    "$FORKD_URL/v1/sandboxes/$sb_id/branch")

  # Detach strace and let it write the summary
  sudo kill -INT "$strace_pid" 2>/dev/null || true
  wait "$strace_pid" 2>/dev/null || true

  pause_ms=$(echo "$resp" | jq -r '.pause_ms // empty')
  diff_ms=$(echo "$resp" | jq -r '.diff_ms // empty')
  diff_phys=$(echo "$resp" | jq -r '.diff_physical_bytes // empty')

  # Pull "total" row from strace -c output. Columns:
  # % time | seconds | usecs/call | calls | errors | syscall
  # We want column 2 (seconds spent in syscalls) → us, and column 4 (calls).
  total_line=$(grep -E "^[0-9.]+\s+[0-9.]+\s+[0-9]+" "$strace_log" | tail -1)
  total_us=$(echo "$total_line" | awk '{printf "%d", $2 * 1000000}')
  total_calls=$(echo "$total_line" | awk '{print $4}')

  echo "$i,$pause_ms,$diff_ms,$diff_phys,$total_us,$total_calls" >> "$OUT_DIR/summary.csv"
  echo "[probe] branch $i: pause=${pause_ms}ms diff=${diff_ms}ms strace_calls=$total_calls" >&2
done

# Cleanup
curl -fsS -X DELETE "${auth[@]}" "$FORKD_URL/v1/sandboxes/$sb_id" > /dev/null || true
echo "" >&2
echo "[probe] done. summary at $OUT_DIR/summary.csv" >&2
echo "[probe] per-branch strace at $OUT_DIR/branch-*.strace" >&2
