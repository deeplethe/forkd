#!/usr/bin/env bash
# sweep-diff.sh — phase 1a A/B for Vm::snapshot_diff_to.
#
# For each memory size: spawn a source, idle briefly, BRANCH once
# with measure_diff=true. The response carries pause_ms (= the Full
# snapshot's pause window, today's behavior) AND diff_ms +
# diff_physical_bytes (= what the same source would have cost if we'd
# taken a Diff snapshot at that pause time, instead of a Full).
#
# Caveat: an idle source has near-zero dirty pages, so diff_ms is the
# best-case estimate. Phase 1b wires diff into a real BRANCH path and
# we re-measure with workloads that actually touch memory between
# BRANCHes.
#
# Usage:
#   ./sweep-diff.sh tmpfs > sweep-diff-tmpfs.csv
#   ./sweep-diff.sh ssd   > sweep-diff-ssd.csv
#
# CSV columns: backend,memory_mib,trial,full_pause_ms,diff_ms,diff_physical_bytes,diff_logical_bytes,diff_ratio_pct
set -euo pipefail

BACKEND=${1:?usage: sweep-diff.sh <tmpfs|ssd>}
FORKD_URL=${FORKD_URL:-http://127.0.0.1:8889}
FORKD_TOKEN=${FORKD_TOKEN:-$(cat "${FORKD_TOKEN_FILE:-/etc/forkd/token}" 2>/dev/null || echo "")}
TAGS=${TAGS:-"mem-256 mem-512 mem-1024 mem-2048 mem-4096"}
TRIALS=${TRIALS:-3}
SETTLE_SECS=${SETTLE_SECS:-3}

auth_header=()
if [[ -n "$FORKD_TOKEN" ]]; then
  auth_header=(-H "Authorization: Bearer $FORKD_TOKEN")
fi

call () { curl -fsS "${auth_header[@]}" -H "Content-Type: application/json" "$@"; }

echo "backend,memory_mib,trial,full_pause_ms,diff_ms,diff_physical_bytes,diff_logical_bytes,diff_ratio_pct"
echo "[sweep-diff] backend=$BACKEND tags=$TAGS trials=$TRIALS settle_secs=$SETTLE_SECS" >&2

for tag in $TAGS; do
  mib=${tag#mem-}
  for trial in $(seq 1 "$TRIALS"); do
    echo "[sweep-diff] tag=$tag trial=$trial" >&2

    spawn_resp=$(call -d "{\"snapshot_tag\":\"$tag\",\"n\":1,\"per_child_netns\":true}" \
      "$FORKD_URL/v1/sandboxes")
    src=$(echo "$spawn_resp" | jq -r '.[0].id')

    sleep "$SETTLE_SECS"

    btag="sweep-diff-${tag}-${trial}-$(date +%s%N)"
    branch_resp=$(call -d "{\"tag\":\"$btag\",\"measure_diff\":true}" \
      "$FORKD_URL/v1/sandboxes/$src/branch")

    pause_ms=$(echo "$branch_resp" | jq -r '.pause_ms // empty')
    diff_ms=$(echo "$branch_resp" | jq -r '.diff_ms // empty')
    diff_phys=$(echo "$branch_resp" | jq -r '.diff_physical_bytes // empty')
    diff_log=$(echo "$branch_resp" | jq -r '.diff_logical_bytes // empty')
    ratio=""
    if [[ -n "$diff_phys" && -n "$diff_log" && "$diff_log" -gt 0 ]]; then
      ratio=$(awk "BEGIN{printf \"%.2f\", $diff_phys * 100 / $diff_log}")
    fi

    echo "$BACKEND,$mib,$trial,$pause_ms,$diff_ms,$diff_phys,$diff_log,$ratio"

    # Cleanup source + BRANCH output to keep /dev/shm under control.
    call -X DELETE "$FORKD_URL/v1/sandboxes/$src" > /dev/null || true
    sudo rm -rf "${FORKD_SNAPSHOT_ROOT:-/home/yangdongxu/.local/share/forkd/snapshots}/$btag" 2>/dev/null || true
  done
done
