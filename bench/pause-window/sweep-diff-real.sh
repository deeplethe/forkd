#!/usr/bin/env bash
# sweep-diff-real.sh — phase 1b A/B for the real diff-mode BRANCH path.
#
# Distinct from sweep-diff.sh (phase 1a, which used measure_diff to time
# a sidecar Diff alongside the Full path). This script invokes the
# real diff path: BRANCH with diff=true does Diff-in-pause + parallel
# memory.bin cp + apply_diff post-resume. The user-visible pause_ms is
# only the Diff window.
#
# Each trial is a FRESH source — diff mode is restricted to the first
# BRANCH per sandbox (see docs/design/diff-snapshots.md "First-BRANCH-
# only restriction"), so multi-BRANCH benchmarking doesn't apply yet.
#
# Usage:
#   ./sweep-diff-real.sh tmpfs > sweep-diff-real-tmpfs.csv
#   ./sweep-diff-real.sh ssd   > sweep-diff-real-ssd.csv
#
# CSV columns: backend,memory_mib,mode,trial,pause_ms,diff_ms,diff_physical_bytes
set -euo pipefail

BACKEND=${1:?usage: sweep-diff-real.sh <tmpfs|ssd>}
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

echo "backend,memory_mib,mode,trial,pause_ms,diff_ms,diff_physical_bytes"
echo "[sweep-diff-real] backend=$BACKEND tags=$TAGS trials=$TRIALS settle_secs=$SETTLE_SECS" >&2

for tag in $TAGS; do
  mib=${tag#mem-}
  for mode in full diff; do
    for trial in $(seq 1 "$TRIALS"); do
      echo "[sweep-diff-real] tag=$tag mode=$mode trial=$trial" >&2

      spawn_resp=$(call -d "{\"snapshot_tag\":\"$tag\",\"n\":1,\"per_child_netns\":true}" \
        "$FORKD_URL/v1/sandboxes")
      src=$(echo "$spawn_resp" | jq -r '.[0].id')

      sleep "$SETTLE_SECS"

      btag="sweep-diff-real-${tag}-${mode}-${trial}-$(date +%s%N)"
      if [[ "$mode" == "diff" ]]; then
        body="{\"tag\":\"$btag\",\"diff\":true}"
      else
        body="{\"tag\":\"$btag\"}"
      fi
      branch_resp=$(call -d "$body" "$FORKD_URL/v1/sandboxes/$src/branch")

      pause_ms=$(echo "$branch_resp" | jq -r '.pause_ms // empty')
      diff_ms=$(echo "$branch_resp" | jq -r '.diff_ms // empty')
      diff_phys=$(echo "$branch_resp" | jq -r '.diff_physical_bytes // empty')

      echo "$BACKEND,$mib,$mode,$trial,$pause_ms,$diff_ms,$diff_phys"

      call -X DELETE "$FORKD_URL/v1/sandboxes/$src" > /dev/null || true
      sudo rm -rf "${FORKD_SNAPSHOT_ROOT:-/home/yangdongxu/.local/share/forkd/snapshots}/$btag" 2>/dev/null || true
    done
  done
done
