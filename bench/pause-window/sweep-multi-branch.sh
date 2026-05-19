#!/usr/bin/env bash
# sweep-multi-branch.sh — N consecutive diff BRANCHes on the same source.
#
# Phase 1d ships multi-BRANCH diff via the previous-output chain. This sweep
# verifies that:
#   1. The Nth diff BRANCH succeeds (no 400, which was phase 1b's behavior).
#   2. Each BRANCH's diff_physical_bytes reflects ONLY pages dirtied since
#      the previous BRANCH (not since restore), because each BRANCH's bitmap
#      clear resets the dirty window.
#   3. pause_ms stays roughly constant across BRANCHes — proves the chain is
#      O(dirty-since-last-BRANCH), not O(total-dirty-since-restore).
#
# Each trial: spawn 1 source, BRANCH it 5 times in a row with `diff: true`,
# 3 s between BRANCHes (lets the kernel dirty some pages between snapshots).
#
# CSV columns: trial,branch_idx,pause_ms,diff_ms,diff_physical_bytes
set -euo pipefail

FORKD_URL=${FORKD_URL:-http://127.0.0.1:8889}
FORKD_TOKEN=${FORKD_TOKEN:-$(cat "${FORKD_TOKEN_FILE:-/etc/forkd/token}" 2>/dev/null || echo "")}
TAG=${TAG:-mem-2048}
N_BRANCHES=${N_BRANCHES:-5}
TRIALS=${TRIALS:-3}
GAP_SECS=${GAP_SECS:-3}

auth_header=()
if [[ -n "$FORKD_TOKEN" ]]; then
  auth_header=(-H "Authorization: Bearer $FORKD_TOKEN")
fi

call () { curl -fsS "${auth_header[@]}" -H "Content-Type: application/json" "$@"; }

echo "trial,branch_idx,pause_ms,diff_ms,diff_physical_bytes"
echo "[sweep-multi-branch] tag=$TAG N=$N_BRANCHES trials=$TRIALS gap=${GAP_SECS}s" >&2

for trial in $(seq 1 "$TRIALS"); do
  echo "[sweep-multi-branch] trial=$trial spawning source" >&2
  spawn=$(call -d "{\"snapshot_tag\":\"$TAG\",\"n\":1,\"per_child_netns\":true}" \
    "$FORKD_URL/v1/sandboxes")
  src=$(echo "$spawn" | jq -r '.[0].id')
  sleep 2

  for i in $(seq 1 "$N_BRANCHES"); do
    sleep "$GAP_SECS"
    btag="multi-${trial}-${i}-$(date +%s%N)"
    resp=$(call -d "{\"tag\":\"$btag\",\"diff\":true}" \
      "$FORKD_URL/v1/sandboxes/$src/branch")
    pause_ms=$(echo "$resp" | jq -r '.pause_ms // empty')
    diff_ms=$(echo "$resp" | jq -r '.diff_ms // empty')
    diff_phys=$(echo "$resp" | jq -r '.diff_physical_bytes // empty')
    echo "$trial,$i,$pause_ms,$diff_ms,$diff_phys"
    # Clean up this BRANCH's snap_dir before the next BRANCH so the daemon
    # doesn't accumulate 2 GiB files per BRANCH.
    # NOTE: the chain head is updated by Registry::mark_branched AFTER the
    # BRANCH completes; deleting the file here breaks the chain so the
    # NEXT BRANCH will fall back to source_tag (logged warning, semantically
    # lossy). For this benchmark we WANT to test the chain stays intact —
    # so we keep the snap_dirs around and rm them at the end of each trial.
    :
  done

  call -X DELETE "$FORKD_URL/v1/sandboxes/$src" > /dev/null || true
  # Sweep all the per-trial snapshots.
  sudo rm -rf "${FORKD_SNAPSHOT_ROOT:-/home/yangdongxu/.local/share/forkd/snapshots}"/multi-"$trial"-* 2>/dev/null || true
done
