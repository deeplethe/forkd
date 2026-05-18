#!/usr/bin/env bash
# Coding-agent branch-and-fan-out demo.
#
# Story:
#   1. Source sandbox sets up a tiny buggy Python package in
#      /tmp/workspace, populates __pycache__ via a failing test run,
#      and writes 50 MB of synthetic "build artifacts".
#   2. BRANCH captures all that state (source code + __pycache__
#      + build artifacts).
#   3. Three grandchildren spawn from the snapshot — each
#      receives a DIFFERENT fix strategy (minimal sed / rewrite /
#      skip-the-test) injected via /tmp/forkd-strategy.sh.
#   4. Each grandchild applies its strategy and re-runs the tests.
#   5. We collect (a) the divergent edits + test outcomes (per-child)
#      and (b) the IDENTICAL build artifacts (proof of CoW state
#      inheritance: md5 matches across all 3 children).
#
# Required env:
#   FORKD_URL              http://127.0.0.1:8889
#   FORKD_TOKEN            bearer token
#
# Optional env:
#   SNAPSHOT_TAG           default: langgraph (any python3 rootfs works)
#   OUT_DIR                default: results/<unix-ts>

set -euo pipefail

: "${FORKD_URL:?FORKD_URL must be set}"
: "${FORKD_TOKEN:?FORKD_TOKEN must be set}"

SNAPSHOT_TAG="${SNAPSHOT_TAG:-langgraph}"
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

# Ship a script's contents to a sandbox + execute it. Encodes via
# base64 so quoting works for arbitrary content.
guest_exec_script() {
  local sandbox_id="$1"
  local script_path="$2"
  local timeout="${3:-30}"
  local enc
  enc=$(base64 -w0 "$script_path")
  local host_now
  host_now=$(date +%s)
  # Wrapper sets clock + tcp_timestamps=0 then runs the script.
  local body
  body=$(jq -nc \
    --arg launch "$enc" \
    --arg host_now "$host_now" \
    --argjson t "$timeout" \
    '{
      args: ["sh","-c",
        ("date -s @" + $host_now + " >/dev/null 2>&1 || true; " +
         "echo 0 > /proc/sys/net/ipv4/tcp_timestamps 2>/dev/null || true; " +
         "echo " + $launch + " | base64 -d | bash " + $host_now)],
      timeout_secs: $t
    }')
  curl_daemon -d "$body" "$FORKD_URL/v1/sandboxes/$sandbox_id/exec"
}

# Read a path inside a sandbox to local file.
guest_read_file() {
  local sandbox_id="$1"
  local path="$2"
  local out_file="$3"
  local body
  body=$(jq -nc --arg p "$path" '{args:["sh","-c", ("cat " + $p)], timeout_secs:15}')
  curl_daemon -d "$body" "$FORKD_URL/v1/sandboxes/$sandbox_id/exec" \
    | jq -r '.stdout // ""' > "$out_file"
}

# Run a small shell command inside a sandbox and return its stdout.
guest_run() {
  local sandbox_id="$1"
  local cmd="$2"
  local enc
  enc=$(printf '%s' "$cmd" | base64 -w0)
  local body
  body=$(jq -nc --arg enc "$enc" '{args:["sh","-c", ("echo " + $enc + " | base64 -d | sh")], timeout_secs:30}')
  curl_daemon -d "$body" "$FORKD_URL/v1/sandboxes/$sandbox_id/exec" | jq -r '.stdout // ""'
}

# ---- 1. Spawn source ----------------------------------------------
echo "[demo] spawning source from snapshot '$SNAPSHOT_TAG'"
SPAWN_RESP=$(curl_daemon \
  -d "{\"snapshot_tag\":\"$SNAPSHOT_TAG\",\"n\":1,\"per_child_netns\":true}" \
  "$FORKD_URL/v1/sandboxes")
SOURCE_ID=$(echo "$SPAWN_RESP" | jq -r '.[0].id')
echo "$SPAWN_RESP" > "$OUT_DIR/spawn.json"
echo "[demo] source id: $SOURCE_ID"
sleep 3

# ---- 2. Run setup-source.sh in the source (background) -------------
# Two-step: (a) write the script to a real file in the guest's
# tmpfs /tmp, then (b) nohup it. Avoids the awkward
# `bash /dev/stdin <arg>` pattern which was silently no-op'ing.
echo "[demo] running setup-source.sh in source (will exit after 60s sleep)"
HOST_NOW=$(date +%s)
SETUP_B64=$(base64 -w0 "$HERE/setup-source.sh")
LAUNCH_BODY=$(jq -nc \
  --arg enc "$SETUP_B64" \
  --arg host_now "$HOST_NOW" \
  '{
    args: ["sh","-c",
      ("echo " + $enc + " | base64 -d > /tmp/setup.sh && chmod +x /tmp/setup.sh && " +
       "nohup /tmp/setup.sh " + $host_now + " >/tmp/setup.log 2>&1 < /dev/null & " +
       "sleep 0.3 && echo started_pid=$! && head -5 /tmp/setup.log 2>/dev/null || true")],
    timeout_secs: 15
  }')
curl_daemon -d "$LAUNCH_BODY" "$FORKD_URL/v1/sandboxes/$SOURCE_ID/exec" > "$OUT_DIR/source-launch.json"
echo "[demo] $(jq -r '.stdout // ""' "$OUT_DIR/source-launch.json")"
echo "[demo] $(jq -r '.stderr // ""' "$OUT_DIR/source-launch.json")"

# ---- 3. Poll for ready_to_branch marker ----------------------------
echo "[demo] waiting for source to reach branch point..."
deadline=$(( $(date +%s) + 90 ))
while [ "$(date +%s)" -lt "$deadline" ]; do
  RESP=$(curl_daemon \
    -d '{"args":["sh","-c","grep -q ready_to_branch /tmp/setup.log && echo READY || echo NOT_YET"],"timeout_secs":10}' \
    "$FORKD_URL/v1/sandboxes/$SOURCE_ID/exec" 2>/dev/null || echo '{"stdout":"poll-fail"}')
  if echo "$RESP" | jq -r '.stdout // ""' 2>/dev/null | grep -q '^READY'; then
    echo "[demo] source reached branch point"
    break
  fi
  sleep 3
done

# Quick proof the source is in the state we expect
echo "[demo] sampling source state at branch point..."
guest_read_file "$SOURCE_ID" "/tmp/workspace/mathy/__init__.py" "$OUT_DIR/source-init-py.txt"
SOURCE_PYCACHE_MD5=$(guest_run "$SOURCE_ID" "find /tmp/workspace -name '__pycache__' -type d | xargs -I{} find {} -type f | sort | xargs md5sum 2>/dev/null | md5sum | awk '{print \$1}'")
SOURCE_VENDORED_MD5=$(guest_run "$SOURCE_ID" "md5sum /tmp/workspace/build-artifacts/vendored.bin | awk '{print \$1}'")
echo "[demo] source __pycache__ tree md5: $SOURCE_PYCACHE_MD5"
echo "[demo] source vendored.bin md5:     $SOURCE_VENDORED_MD5"

# ---- 4. BRANCH -----------------------------------------------------
BRANCH_TAG="coding-fork-$(date +%s)"
echo "[demo] BRANCH → tag=$BRANCH_TAG"
T0=$(date +%s%3N)
BRANCH_RESP=$(curl_daemon \
  -d "{\"tag\":\"$BRANCH_TAG\"}" \
  "$FORKD_URL/v1/sandboxes/$SOURCE_ID/branch")
T1=$(date +%s%3N)
echo "$BRANCH_RESP" > "$OUT_DIR/branch.json"
DAEMON_PAUSE_MS=$(echo "$BRANCH_RESP" | jq -r '.pause_ms')
echo "[demo] daemon pause_ms=$DAEMON_PAUSE_MS  wall=$(( T1 - T0 )) ms"

# ---- 5. Spawn 3 grandchildren --------------------------------------
echo "[demo] spawning 3 grandchildren"
GRANDS=$(curl_daemon \
  -d "{\"snapshot_tag\":\"$BRANCH_TAG\",\"n\":3,\"per_child_netns\":true}" \
  "$FORKD_URL/v1/sandboxes")
echo "$GRANDS" > "$OUT_DIR/grandchildren.json"

CHILD_MIN=$(echo "$GRANDS" | jq -r '.[0].id')
CHILD_REW=$(echo "$GRANDS" | jq -r '.[1].id')
CHILD_SKP=$(echo "$GRANDS" | jq -r '.[2].id')

declare -A STRATEGIES LABELS
STRATEGIES["$CHILD_MIN"]="$HERE/strategies/minimal.sh"
STRATEGIES["$CHILD_REW"]="$HERE/strategies/rewrite.sh"
STRATEGIES["$CHILD_SKP"]="$HERE/strategies/skip.sh"
LABELS["$CHILD_MIN"]="minimal"
LABELS["$CHILD_REW"]="rewrite"
LABELS["$CHILD_SKP"]="skip"

# ---- 6. Inject each child's strategy + run it ----------------------
for id in "$CHILD_MIN" "$CHILD_REW" "$CHILD_SKP"; do
  label="${LABELS[$id]}"
  script="${STRATEGIES[$id]}"
  echo "[demo] running strategy '$label' in $id"
  guest_exec_script "$id" "$script" 60 > "$OUT_DIR/child-$label-exec.json"
done

# Give each child a moment to finish its test run
sleep 5

# ---- 7. Collect evidence -------------------------------------------
echo "[demo] collecting evidence"
for entry in "source $SOURCE_ID" "minimal $CHILD_MIN" "rewrite $CHILD_REW" "skip $CHILD_SKP"; do
  label=${entry%% *}; id=${entry##* }
  guest_read_file "$id" "/tmp/workspace/mathy/__init__.py" "$OUT_DIR/$label-init-py.txt"
  guest_read_file "$id" "/tmp/workspace/.agent-log" "$OUT_DIR/$label-agent.log"
  pycache_md5=$(guest_run "$id" "find /tmp/workspace -name '__pycache__' -type d | xargs -I{} find {} -type f | sort | xargs md5sum 2>/dev/null | md5sum | awk '{print \$1}'")
  vendored_md5=$(guest_run "$id" "md5sum /tmp/workspace/build-artifacts/vendored.bin | awk '{print \$1}'")
  vendored_size=$(guest_run "$id" "stat -c '%s' /tmp/workspace/build-artifacts/vendored.bin")
  echo "$label $id $pycache_md5 $vendored_md5 $vendored_size" >> "$OUT_DIR/state-evidence.txt"
done

cat "$OUT_DIR/state-evidence.txt"

# ---- 8. Teardown ---------------------------------------------------
echo "[demo] tearing down sandboxes"
for id in "$SOURCE_ID" "$CHILD_MIN" "$CHILD_REW" "$CHILD_SKP"; do
  curl -fsS -X DELETE -H "Authorization: Bearer $FORKD_TOKEN" \
    "$FORKD_URL/v1/sandboxes/$id" >/dev/null 2>&1 || true
done

# ---- 9. Render summary ---------------------------------------------
python3 "$HERE/summarize.py" \
  --out-dir "$OUT_DIR" \
  --daemon-pause-ms "$DAEMON_PAUSE_MS" \
  --branch-tag "$BRANCH_TAG" \
  --source-id "$SOURCE_ID" \
  --child-minimal "$CHILD_MIN" \
  --child-rewrite "$CHILD_REW" \
  --child-skip "$CHILD_SKP"

echo
echo "[demo] done. See $OUT_DIR/summary.md"
