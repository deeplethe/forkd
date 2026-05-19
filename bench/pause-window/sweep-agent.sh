#!/usr/bin/env bash
# sweep-agent.sh — measure Full vs Diff BRANCH against a source that
# has a controlled dirty footprint.
#
# Idle-source phase 1b numbers (RESULTS-v0.3.md) showed Diff at 143× on
# 4 GiB SSD because the dirty footprint was ~900 KiB. Real fan-out
# workloads dirty more — at some dirty-MiB threshold Diff loses its
# advantage because the diff write itself becomes the cost. This sweep
# finds that threshold by dialing dirty footprint deterministically
# via the dirtier.py guest workload.
#
# For each (mem-size, dirty-mib): spawn source, exec dirtier.py inside
# guest, wait for "READY_TO_BRANCH" marker on stdout, BRANCH once.
# 3 trials per cell, modes ∈ {full, diff} (diff is forkd v0.3 phase 1b's
# `"diff": true` request).
#
# CSV columns: backend,memory_mib,dirty_mib,mode,trial,pause_ms,diff_ms,diff_physical_bytes
#
# Usage:
#   FORKD_TOKEN=$(cat /tmp/bench-pause/token) \
#     ./sweep-agent.sh ssd > sweep-agent-ssd.csv
set -euo pipefail

BACKEND=${1:?usage: sweep-agent.sh <tmpfs|ssd>}
FORKD_URL=${FORKD_URL:-http://127.0.0.1:8889}
FORKD_TOKEN=${FORKD_TOKEN:-$(cat "${FORKD_TOKEN_FILE:-/etc/forkd/token}" 2>/dev/null || echo "")}

# Only one source size by default — the variable we're sweeping here is
# dirty footprint, not source size (RESULTS-v0.3.md already swept size).
# 2 GiB is the realistic agent size and leaves room for up to ~1.5 GiB
# dirty without saturating.
TAGS=${TAGS:-"mem-2048"}
# Dirty footprints in MiB. 0 = idle (control, matches RESULTS-v0.3.md).
DIRTY=${DIRTY:-"0 10 50 100 250 500 1000"}
TRIALS=${TRIALS:-3}

# Where dirtier.py lives on the dev box (this script runs there).
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DIRTIER="$HERE/dirtier.py"
[ -f "$DIRTIER" ] || { echo "dirtier.py not found at $DIRTIER" >&2; exit 1; }
DIRTIER_B64=$(base64 -w0 "$DIRTIER")

auth_header=()
if [[ -n "$FORKD_TOKEN" ]]; then
  auth_header=(-H "Authorization: Bearer $FORKD_TOKEN")
fi

call () { curl -fsS "${auth_header[@]}" -H "Content-Type: application/json" "$@"; }

# Marker poll: keeps re-execing a small `tail` on the agent's stdout
# log inside the guest. Returns when the marker is present.
wait_for_marker () {
  local src="$1"
  local stdout_path="$2"
  local deadline_s="${3:-90}"
  local end=$((SECONDS + deadline_s))
  while (( SECONDS < end )); do
    body=$(jq -nc --arg p "$stdout_path" '{args:["sh","-c","grep -q READY_TO_BRANCH " + $p + " && echo ok"], timeout_secs: 5}')
    resp=$(call -d "$body" "$FORKD_URL/v1/sandboxes/$src/exec" 2>/dev/null || true)
    if echo "$resp" | jq -r '.stdout // empty' | grep -q '^ok$'; then
      return 0
    fi
    sleep 0.2
  done
  echo "[sweep-agent] WARN: marker not seen in ${deadline_s}s for $src" >&2
  return 1
}

echo "backend,memory_mib,dirty_mib,mode,trial,pause_ms,diff_ms,diff_physical_bytes"
echo "[sweep-agent] backend=$BACKEND tags=$TAGS dirty=$DIRTY trials=$TRIALS" >&2

for tag in $TAGS; do
  mib=${tag#mem-}
  for dirty in $DIRTY; do
    for mode in full diff; do
      for trial in $(seq 1 "$TRIALS"); do
        echo "[sweep-agent] tag=$tag dirty=${dirty}MiB mode=$mode trial=$trial" >&2

        spawn=$(call -d "{\"snapshot_tag\":\"$tag\",\"n\":1,\"per_child_netns\":true}" \
          "$FORKD_URL/v1/sandboxes")
        src=$(echo "$spawn" | jq -r '.[0].id')

        # Wait for guest agent to come up (sandbox just spawned).
        sleep 2

        # Kick off the dirtier in the background inside the guest.
        # Hold-s = 60 s; plenty for any BRANCH path.
        stdout_path="/tmp/dirtier-$trial.out"
        cmd="echo $DIRTIER_B64 | base64 -d | python3 - --dirty-mib $dirty --hold-s 60 > $stdout_path 2>&1 &"
        body=$(jq -nc --arg c "$cmd" '{args:["sh","-c", $c + " disown ; echo started"], timeout_secs: 10}')
        call -d "$body" "$FORKD_URL/v1/sandboxes/$src/exec" > /dev/null

        # Wait for the dirtier to finish writing pages.
        wait_for_marker "$src" "$stdout_path" 90 || {
          call -X DELETE "$FORKD_URL/v1/sandboxes/$src" > /dev/null || true
          continue
        }

        # BRANCH now — source has exactly $dirty MiB dirty.
        btag="sweep-agent-${tag}-d${dirty}-${mode}-${trial}-$(date +%s%N)"
        if [[ "$mode" == "diff" ]]; then
          body="{\"tag\":\"$btag\",\"diff\":true}"
        else
          body="{\"tag\":\"$btag\"}"
        fi
        resp=$(call -d "$body" "$FORKD_URL/v1/sandboxes/$src/branch")
        pause_ms=$(echo "$resp" | jq -r '.pause_ms // empty')
        diff_ms=$(echo "$resp" | jq -r '.diff_ms // empty')
        diff_phys=$(echo "$resp" | jq -r '.diff_physical_bytes // empty')

        echo "$BACKEND,$mib,$dirty,$mode,$trial,$pause_ms,$diff_ms,$diff_phys"

        call -X DELETE "$FORKD_URL/v1/sandboxes/$src" > /dev/null || true
        sudo rm -rf "${FORKD_SNAPSHOT_ROOT:-/home/yangdongxu/.local/share/forkd/snapshots}/$btag" 2>/dev/null || true
      done
    done
  done
done
