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
LLM_MODEL="${LLM_MODEL:-deepseek-ai/DeepSeek-V3}"
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
# Before starting the agent, sync the guest's wall clock to the
# host's. A snapshot-restored VM keeps the snapshot-time clock,
# and a multi-minute skew vs host wall-time was enough to make
# TLS to api.siliconflow.cn hang forever (TCP timestamp / PAWS
# rejection of packets whose host-side timestamps look "future").
echo "[demo] launching agent (background)"
HOST_NOW=$(date +%s)
LAUNCH_SCRIPT=$(cat <<EOS
set -e
date -s "@$HOST_NOW" >/dev/null 2>&1 || true
# Disable TCP timestamps so PAWS (RFC 7323) doesn't drop packets
# when the guest's TCP timestamp counter looks stale relative to
# the host's. With the snapshot frozen in time, even the post
# date-sync wall clock leaves the kernel's internal TCP timestamp
# counter (jiffies-based) behind; the safe fix is to opt out.
# Use /proc directly since python:slim doesn't ship sysctl(8).
echo 0 > /proc/sys/net/ipv4/tcp_timestamps 2>/dev/null || true
# Pre-warm conntrack: poke api.siliconflow.cn before the agent runs.
# The first connection from a freshly-restored sandbox sometimes
# hangs the whole TLS handshake on the host's conntrack table.
# A throwaway curl establishes the conntrack entry and subsequent
# calls succeed immediately.
python3 -c "import socket,ssl; s=ssl.create_default_context().wrap_socket(socket.socket(),server_hostname='api.siliconflow.cn'); s.settimeout(10); s.connect(('api.siliconflow.cn',443)); s.close()" 2>/dev/null || true
mkdir -p /tmp
: > /tmp/forkd-hint.txt
: > /tmp/forkd-agent-stdout.log
export LLM_API_KEY='$SILICONFLOW_API_KEY'
export LLM_MODEL='$LLM_MODEL'
cd /opt/forkd-demo
nohup python3 agent.py \
  --branch-after-step $BRANCH_AFTER_STEP \
  --branch-wait-s $BRANCH_WAIT_S \
  --max-steps 5 \
  --temperature 0.2 \
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
# Hint text often contains $ characters (e.g. "$$$" for "expensive").
# Passing through two layers of shell would mangle them, so we
# base64-encode the hint, ship the encoded blob, and decode
# server-side. Bulletproof against any byte the hint contains.
for id in "$CHILD_A" "$CHILD_B" "$CHILD_C"; do
  label="${LABELS[$id]}"
  hint="${HINTS[$id]}"
  hint_b64=$(printf '%s\n' "$hint" | base64 -w0)
  HOST_NOW=$(date +%s)
  echo "[demo] hint → $label ($id)"
  # Sync clock first (same reason as the source launch). Each
  # grandchild's clock is restored from the BRANCH snapshot's
  # timestamp; left alone, the same TLS-hang would hit each agent
  # the moment it makes its next HTTP call after waking up from
  # time.sleep().
  guest_exec "$id" "date -s '@$HOST_NOW' >/dev/null 2>&1 || true; echo 0 > /proc/sys/net/ipv4/tcp_timestamps 2>/dev/null || true; python3 -c 'import socket,ssl; s=ssl.create_default_context().wrap_socket(socket.socket(),server_hostname=\"api.siliconflow.cn\"); s.settimeout(10); s.connect((\"api.siliconflow.cn\",443)); s.close()' 2>/dev/null || true; echo $hint_b64 | base64 -d > /tmp/forkd-hint.txt && wc -c /tmp/forkd-hint.txt" 30 \
    > "$OUT_DIR/child-$label-hint.json"
done

# Also save the parent's "no hint" state for symmetry.
guest_exec "$SOURCE_ID" "echo 'no hint (parent control)' > /tmp/forkd-hint-meta.txt && echo ok" 15 > /dev/null

# ---- 7. Wait for in-flight sleep + remaining steps to finish ---
# Generous tail: BRANCH_WAIT_S to wake from the in-flight time.sleep,
# then ~3 minutes more for the agents to grind through their
# remaining steps. The chat-completion retries can chew a full
# minute each on stale-conntrack runs, so 5 steps × 60s ≈ 5 min
# is the safe budget.
echo "[demo] waiting ${BRANCH_WAIT_S}s for branch sleep to expire + ~3 min for agents to finish loop..."
sleep $(( BRANCH_WAIT_S + 180 ))

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
