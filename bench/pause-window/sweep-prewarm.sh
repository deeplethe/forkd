#!/usr/bin/env bash
# sweep-prewarm.sh — measure BRANCH pause across memory sizes with and
# without the new post-restore prewarm. Output is one CSV row per trial
# to stdout so it composes with `tee`/`awk` for analysis.
#
# Usage:
#   ./sweep-prewarm.sh tmpfs > tmpfs-prewarm-sweep.csv
#   ./sweep-prewarm.sh ssd   > ssd-prewarm-sweep.csv
#
# Assumes:
#   - The daemon is already running with --snapshot-root pointing at the
#     storage backend you're measuring (re-start it pointing at /dev/shm
#     before the tmpfs run; at the SSD path before the ssd run).
#   - Snapshots tagged mem-256, mem-512, mem-1024, mem-2048, mem-4096
#     have already been built (see RESULTS-v0.2.md "Memory size sweep").
#   - FORKD_TOKEN / FORKD_URL env vars are set (or override via flags).
#
# CSV columns: backend,memory_mib,prewarm,trial,pause_ms,prewarm_ms
#
# `pause_ms` is the daemon's measured pause envelope (PATCH /vm pause →
# PATCH /vm resume) on the source VM. `prewarm_ms` is the wall-clock
# cost of the prewarm pass during sandbox creation; only populated when
# prewarm=true.
set -euo pipefail

BACKEND=${1:?usage: sweep-prewarm.sh <tmpfs|ssd>}
FORKD_URL=${FORKD_URL:-http://127.0.0.1:8889}
FORKD_TOKEN=${FORKD_TOKEN:-$(cat "${FORKD_TOKEN_FILE:-/etc/forkd/token}" 2>/dev/null || echo "")}
TAGS=${TAGS:-"mem-256 mem-512 mem-1024 mem-2048 mem-4096"}
TRIALS=${TRIALS:-3}
SETTLE_SECS=${SETTLE_SECS:-2}

auth_header=()
if [[ -n "$FORKD_TOKEN" ]]; then
  auth_header=(-H "Authorization: Bearer $FORKD_TOKEN")
fi

call () {
  curl -fsS "${auth_header[@]}" -H "Content-Type: application/json" "$@"
}

# Emit CSV header on stdout once. Run notes go to stderr so they don't
# pollute the CSV pipe.
echo "backend,memory_mib,prewarm,trial,pause_ms,prewarm_ms"
echo "[sweep] backend=$BACKEND tags=$TAGS trials=$TRIALS" >&2

for tag in $TAGS; do
  mib=${tag#mem-}
  for prewarm in true false; do
    for trial in $(seq 1 $TRIALS); do
      echo "[sweep] tag=$tag prewarm=$prewarm trial=$trial" >&2

      # Spawn source. Daemon does prewarm inline if prewarm=true.
      spawn_resp=$(call -d "{\"snapshot_tag\":\"$tag\",\"n\":1,\"per_child_netns\":true,\"prewarm\":$prewarm}" \
        "$FORKD_URL/v1/sandboxes")
      src=$(echo "$spawn_resp" | jq -r '.[0].id')

      # prewarm_ms isn't exposed in the response body today; the daemon
      # logs it via tracing. Best-effort grab from the journal — empty
      # if not running under systemd. The pause_ms below is the value
      # that actually matters for this experiment.
      prewarm_ms=""
      if [[ "$prewarm" == "true" ]]; then
        prewarm_ms=$(journalctl -u forkd-controller -n 50 --no-pager 2>/dev/null \
          | grep -F "$src" | grep -oE 'prewarm_ms=[0-9]+' | tail -1 | cut -d= -f2 || true)
      fi

      sleep "$SETTLE_SECS"

      btag="sweep-${tag}-${prewarm}-${trial}-$(date +%s%N)"
      branch_resp=$(call -d "{\"tag\":\"$btag\"}" \
        "$FORKD_URL/v1/sandboxes/$src/branch")
      pause_ms=$(echo "$branch_resp" | jq -r '.pause_ms // empty')

      echo "$BACKEND,$mib,$prewarm,$trial,$pause_ms,$prewarm_ms"

      # Cleanup so the next trial starts from a fresh source. The
      # branch snapshot stays on disk for now; rm it manually after the
      # sweep if you want the disk back.
      call -X DELETE "$FORKD_URL/v1/sandboxes/$src" > /dev/null || true
    done
  done
done
