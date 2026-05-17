#!/usr/bin/env bash
# Branch-and-fan-out demo orchestrator.
#
# Flow:
#
#   1. Spawn source sandbox from `langgraph` snapshot
#   2. Start agent.py in source via `nohup &` so the exec returns
#      immediately; agent logs to /tmp/forkd-agent-stdout.log
#   3. Poll for the `ready_to_branch` marker in the log file (via
#      a follow-up exec that greps it)
#   4. POST /branch → new tag `langgraph-fork-<ts>`
#   5. Spawn 3 grandchildren from that tag — each inherits the
#      paused agent process mid-time.sleep()
#   6. Plant a different hint in each via exec
#   7. Wait for time.sleep() to expire (~45s); agents continue,
#      read their hints, finish their loops
#   8. Collect each transcript by cat'ing the log file
#   9. Run summarize.py to emit summary.md
#
# Required env:
#   FORKD_URL              http://127.0.0.1:8889
#   FORKD_TOKEN            bearer token
#   SILICONFLOW_API_KEY    LLM key, propagated into each sandbox

set -euo pipefail

: "${FORKD_URL:?FORKD_URL must be set}"
: "${FORKD_TOKEN:?FORKD_TOKEN must be set}"
: "${SILICONFLOW_API_KEY:?SILICONFLOW_API_KEY must be set}"

SNAPSHOT_TAG="${SNAPSHOT_TAG:-langgraph}"
LLM_MODEL="${LLM_MODEL:-Qwen/Qwen2.5-7B-Instruct}"
BRANCH_AFTER_STEP="${BRANCH_AFTER_STEP:-3}"
BRANCH_WAIT_S="${BRANCH_WAIT_S:-45}"
OUT_DIR="${OUT_DIR:-results/$(date +%s)}"
mkdir -p "$OUT_DIR"
echo "[demo] writing artifacts to $OUT_DIR"

HERE="$(cd "$(dirname "$0")" && pwd)"

curl_daemon() {
  curl -fsS \
    -H "Authorization: Bearer $FORKD_TOKEN" \
    -H "Content-Type: application/json" \
    "$@"
}

# Run a bash one-liner inside a sandbox via the daemon's exec API.
# We base64-encode the script body so we don't fight JSON quoting.
guest_exec() {
  local sandbox_id="$1"
  local script="$2"
  local timeout="${3:-30}"
  local enc
  enc=$(printf '%s' "$script" | base64 -w0)
  local body
  body=$(jq -nc --arg launch "$enc" --argjson t "$timeout" '{
    args: ["sh","-c", ("echo " + $launch + " | base64 -d | bash")],
    timeout_secs: $t
  }')
  curl_daemon -d "$body" "$FORKD_URL/v1/sandboxes/$sandbox_id/exec"
}

# ---- 1. Spawn source --------------------------------------------
echo "[demo] spawning source from snapshot '$SNAPSHOT_TAG'"
SPAWN_RESP=$(curl_daemon \
  -d "{\"snapshot_tag\":\"$SNAPSHOT_TAG\",\"n\":1,\"per_child_netns\":true}" \
  "$FORKD_URL/v1/sandboxes")
SOURCE_ID=$(echo "$SPAWN_RESP" | jq -r '.[0].id')
echo "$SPAWN_RESP" > "$OUT_DIR/spawn.json"
echo "[demo] source id: $SOURCE_ID"
sleep 3

# ---- 2. Launch agent in background ------------------------------
echo "[demo] launching agent (background)"
LAUNCH_SCRIPT=$(cat <<EOS
set -e
mkdir -p /tmp
: > /tmp/forkd-hint.txt
: > /tmp/forkd-agent-stdout.log
export LLM_API_KEY='$SILICONFLOW_API_KEY'
export LLM_MODEL='$LLM_MODEL'
cd /opt/forkd-demo
nohup python3 agent.py \
  --branch-after-step $BRANCH_AFTER_STEP \
  --branch-wait-s $BRANCH_WAIT_S \
  --max-steps 8 \
  >/tmp/forkd-agent-stdout.log 2>&1 < /dev/null &
echo "agent pid=\$!"
EOS
)
guest_exec "$SOURCE_ID" "$LAUNCH_SCRIPT" 30 > "$OUT_DIR/source-launch.json"
echo "[demo] $(jq -r '.stdout // ""' "$OUT_DIR/source-launch.json")"

# ---- 3. Poll for ready_to_branch marker -------------------------
echo "[demo] waiting for agent to reach branch point..."
deadline=$(( $(date +%s) + 120 ))
while [ "$(date +%s)" -lt "$deadline" ]; do
  RESP=$(guest_exec "$SOURCE_ID" 'grep -q ready_to_branch /tmp/forkd-agent-stdout.log && echo READY || echo NOT_YET' 15 2>/dev/null || echo '{"stdout":"poll-fail"}')
  if echo "$RESP" | jq -r '.stdout // ""' 2>/dev/null | grep -q '^READY'; then
    echo "[demo] source reached branch point"
    break
  fi
  sleep 3
done

# ---- 4. BRANCH --------------------------------------------------
BRANCH_TAG="langgraph-fork-$(date +%s)"
echo "[demo] BRANCH → tag=$BRANCH_TAG"
T0=$(date +%s%3N)
BRANCH_RESP=$(curl_daemon \
  -d "{\"tag\":\"$BRANCH_TAG\"}" \
  "$FORKD_URL/v1/sandboxes/$SOURCE_ID/branch")
T1=$(date +%s%3N)
echo "$BRANCH_RESP" > "$OUT_DIR/branch.json"
DAEMON_PAUSE_MS=$(echo "$BRANCH_RESP" | jq -r '.pause_ms')
echo "[demo] daemon pause_ms=$DAEMON_PAUSE_MS  wall=$(( T1 - T0 )) ms"

# ---- 5. Spawn 3 grandchildren -----------------------------------
echo "[demo] spawning 3 grandchildren"
GRANDS=$(curl_daemon \
  -d "{\"snapshot_tag\":\"$BRANCH_TAG\",\"n\":3,\"per_child_netns\":true}" \
  "$FORKD_URL/v1/sandboxes")
echo "$GRANDS" > "$OUT_DIR/grandchildren.json"

CHILD_A=$(echo "$GRANDS" | jq -r '.[0].id')
CHILD_B=$(echo "$GRANDS" | jq -r '.[1].id')
CHILD_C=$(echo "$GRANDS" | jq -r '.[2].id')

declare -A HINTS LABELS
HINTS["$CHILD_A"]="Be thorough. Maximize cultural depth — slow down, prefer fewer stops with longer visits."
HINTS["$CHILD_B"]="Be minimal. Maximize daylight outside — fewer indoor stops, no shopping streets."
HINTS["$CHILD_C"]="Optimize for cost. Avoid \$\$\$ items entirely; prefer free or \$."
LABELS["$CHILD_A"]="thorough"
LABELS["$CHILD_B"]="minimal"
LABELS["$CHILD_C"]="cost"

# ---- 6. Plant a hint into each child ---------------------------
for id in "$CHILD_A" "$CHILD_B" "$CHILD_C"; do
  label="${LABELS[$id]}"
  hint="${HINTS[$id]}"
  echo "[demo] hint → $label ($id)"
  guest_exec "$id" "printf '%s\n' \"$hint\" > /tmp/forkd-hint.txt && echo ok" 20 \
    > "$OUT_DIR/child-$label-hint.json"
done

# Also save the parent's "no hint" state for symmetry.
guest_exec "$SOURCE_ID" "echo 'no hint (parent control)' > /tmp/forkd-hint-meta.txt && echo ok" 15 > /dev/null

# ---- 7. Wait for in-flight sleep + remaining steps to finish ---
echo "[demo] waiting ${BRANCH_WAIT_S}s for branch sleep to expire + agents to finish loop..."
sleep $(( BRANCH_WAIT_S + 30 ))

# ---- 8. Collect transcripts -------------------------------------
echo "[demo] collecting transcripts"
COLLECT_SCRIPT='cat /tmp/forkd-agent-stdout.log 2>/dev/null || echo {"event":"error","what":"no log"}'

for entry in "source-$SOURCE_ID-parent" "child-$CHILD_A-thorough" "child-$CHILD_B-minimal" "child-$CHILD_C-cost"; do
  id="${entry#*-}"; id="${id%-*}"
  label="${entry##*-}"
  prefix="${entry%%-*}"
  out_file="$OUT_DIR/${prefix}-${label}-transcript.jsonl"
  echo "[demo]   $label ($id) → $out_file"
  guest_exec "$id" "$COLLECT_SCRIPT" 30 > "$OUT_DIR/${prefix}-${label}-exec.json"
  jq -r '.stdout // ""' "$OUT_DIR/${prefix}-${label}-exec.json" > "$out_file"
done

# ---- 9. Teardown ------------------------------------------------
echo "[demo] tearing down sandboxes"
for id in "$SOURCE_ID" "$CHILD_A" "$CHILD_B" "$CHILD_C"; do
  curl -fsS -X DELETE -H "Authorization: Bearer $FORKD_TOKEN" \
    "$FORKD_URL/v1/sandboxes/$id" >/dev/null 2>&1 || true
done

# ---- 10. Summary -----------------------------------------------
python3 "$HERE/summarize.py" \
  --out-dir "$OUT_DIR" \
  --daemon-pause-ms "$DAEMON_PAUSE_MS" \
  --branch-tag "$BRANCH_TAG" \
  --source-id "$SOURCE_ID" \
  --child-thorough "$CHILD_A" \
  --child-minimal "$CHILD_B" \
  --child-cost "$CHILD_C"

echo
echo "[demo] done. See $OUT_DIR/summary.md"
