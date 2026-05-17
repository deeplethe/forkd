#!/usr/bin/env bash
# Pause-window benchmark — single-trial orchestrator.
#
# Runs ONE trial against a pre-existing forkd-controller daemon
# and a snapshot tag of your choice. Spins up an echo server on
# the host, spawns a sandbox from the snapshot, launches the
# agent inside it via `forkd-controller exec`, triggers BRANCH
# mid-run, collects logs, and runs the analyzer.
#
# Out-of-scope (deliberately):
#   - Building the rootfs / snapshot. Use the existing
#     `recipes/postgres-fixture/` or build your own; pass the tag
#     via --snapshot-tag.
#   - Multi-trial sweeps across memory sizes. Wrap this script.
#   - Cross-host coordination. Run on the box where the daemon is.
#
# Requires: bash, python3, curl, jq, an authenticated
# forkd-controller running on $FORKD_URL with bearer $FORKD_TOKEN.

set -euo pipefail

# ---- defaults ---------------------------------------------------------------
: "${FORKD_URL:=http://127.0.0.1:8889}"
: "${FORKD_TOKEN:?FORKD_TOKEN must be set (the daemon bearer token)}"

SNAPSHOT_TAG=""
ECHO_PORT=39999
INTERVAL_MS=100
DURATION_S=60
BRANCH_AT_S=30
READ_TIMEOUT_MS=30000
OUT_DIR=""
PYTHON_BIN="python3"
ECHO_HOST=""  # auto-detected below

usage() {
  cat <<EOF
Usage: $0 --snapshot-tag <tag> [options]

Required:
  --snapshot-tag <tag>      Source snapshot tag to spawn from

Common options:
  --out <dir>               Output dir (default: results/<tag>-<unix-ts>/)
  --duration-s <n>          Agent runtime (default 60)
  --branch-at-s <n>         Trigger BRANCH N seconds in (default 30)
  --interval-ms <n>         Agent ping cadence (default 100)
  --read-timeout-ms <n>     Agent socket read timeout (default 30000)
  --echo-port <n>           Echo server port (default 39999)
  --echo-host <ip>          Agent-visible host IP for echo server.
                            Defaults to the address of the agent-facing
                            bridge (forkd-br0). Required if auto-detect
                            fails.

Environment:
  FORKD_URL     Controller URL (default $FORKD_URL)
  FORKD_TOKEN   Bearer token (required)
EOF
  exit 1
}

# ---- arg parsing ------------------------------------------------------------
while [[ $# -gt 0 ]]; do
  case "$1" in
    --snapshot-tag)    SNAPSHOT_TAG="$2"; shift 2;;
    --out)             OUT_DIR="$2"; shift 2;;
    --duration-s)      DURATION_S="$2"; shift 2;;
    --branch-at-s)     BRANCH_AT_S="$2"; shift 2;;
    --interval-ms)     INTERVAL_MS="$2"; shift 2;;
    --read-timeout-ms) READ_TIMEOUT_MS="$2"; shift 2;;
    --echo-port)       ECHO_PORT="$2"; shift 2;;
    --echo-host)       ECHO_HOST="$2"; shift 2;;
    -h|--help)         usage;;
    *) echo "unknown arg: $1" >&2; usage;;
  esac
done
[[ -z "$SNAPSHOT_TAG" ]] && usage

# Auto-detect echo host = the host's IP on the forkd bridge. Adjust
# the interface name if your deployment uses something different.
if [[ -z "$ECHO_HOST" ]]; then
  if ip -4 addr show forkd-br0 >/dev/null 2>&1; then
    ECHO_HOST=$(ip -4 addr show forkd-br0 | awk '/inet /{print $2}' | cut -d/ -f1 | head -1)
  fi
fi
if [[ -z "$ECHO_HOST" ]]; then
  echo "Could not auto-detect --echo-host. Pass it explicitly." >&2
  exit 2
fi

# ---- output dir -------------------------------------------------------------
if [[ -z "$OUT_DIR" ]]; then
  OUT_DIR="results/${SNAPSHOT_TAG}-$(date +%s)"
fi
mkdir -p "$OUT_DIR"
echo "[run] writing artifacts to $OUT_DIR"

HERE="$(cd "$(dirname "$0")" && pwd)"

# ---- echo server (background) ----------------------------------------------
echo "[run] starting echo server on $ECHO_HOST:$ECHO_PORT"
"$PYTHON_BIN" "$HERE/echo_server.py" \
  --host "$ECHO_HOST" --port "$ECHO_PORT" --accept-one \
  > "$OUT_DIR/server.jsonl" 2>&1 &
ECHO_PID=$!
trap 'kill $ECHO_PID 2>/dev/null || true' EXIT

# Wait until the listen line is in the log (max 5s).
for _ in $(seq 1 50); do
  if grep -q '"event":"listen"' "$OUT_DIR/server.jsonl" 2>/dev/null; then break; fi
  sleep 0.1
done

# ---- spawn source sandbox --------------------------------------------------
echo "[run] spawning source sandbox from snapshot '$SNAPSHOT_TAG'"
SPAWN_BODY=$(printf '{"snapshot_tag":"%s","n":1,"per_child_netns":true}' "$SNAPSHOT_TAG")
SPAWN_RESP=$(curl -fsS -H "Authorization: Bearer $FORKD_TOKEN" \
  -H "Content-Type: application/json" \
  -d "$SPAWN_BODY" \
  "$FORKD_URL/v1/sandboxes")
SOURCE_ID=$(echo "$SPAWN_RESP" | jq -r '.[0].id')
echo "[run] source id: $SOURCE_ID"
echo "$SPAWN_RESP" > "$OUT_DIR/spawn.json"

# Give the guest agent a moment to settle.
sleep 2

# ---- exec the agent inside the sandbox (blocking) --------------------------
AGENT_SCRIPT=$(cat "$HERE/agent.py" | base64 -w0)
echo "[run] starting agent (duration ${DURATION_S}s, branch at ${BRANCH_AT_S}s)"
EXEC_BODY=$(jq -nc \
  --argjson timeout $(( DURATION_S + 30 )) \
  --arg script "$AGENT_SCRIPT" \
  --arg host "$ECHO_HOST" \
  --arg port "$ECHO_PORT" \
  --arg interval "$INTERVAL_MS" \
  --arg duration "$DURATION_S" \
  --arg timeout_ms "$READ_TIMEOUT_MS" \
  '{
    args: ["sh", "-c", "echo " + $script + " | base64 -d | python3 - --host " + $host + " --port " + $port + " --interval-ms " + $interval + " --duration-s " + $duration + " --read-timeout-ms " + $timeout_ms],
    timeout_secs: $timeout
  }')

# Fire the agent in the background so we can BRANCH while it runs.
(
  curl -fsS -H "Authorization: Bearer $FORKD_TOKEN" \
    -H "Content-Type: application/json" \
    -d "$EXEC_BODY" \
    "$FORKD_URL/v1/sandboxes/$SOURCE_ID/exec" \
    > "$OUT_DIR/exec.json"
) &
EXEC_PID=$!

# ---- trigger BRANCH at the chosen offset -----------------------------------
sleep "$BRANCH_AT_S"
BRANCH_TAG="branch-pause-$(date +%s)"
echo "[run] triggering BRANCH → tag=$BRANCH_TAG"
BRANCH_T_BEFORE=$(date +%s%3N)
BRANCH_RESP=$(curl -fsS -H "Authorization: Bearer $FORKD_TOKEN" \
  -H "Content-Type: application/json" \
  -d "{\"tag\":\"$BRANCH_TAG\"}" \
  "$FORKD_URL/v1/sandboxes/$SOURCE_ID/branch")
BRANCH_T_AFTER=$(date +%s%3N)
echo "$BRANCH_RESP" > "$OUT_DIR/branch.json"
DAEMON_PAUSE_MS=$(echo "$BRANCH_RESP" | jq -r '.pause_ms // empty')
WALL_BRANCH_MS=$(( BRANCH_T_AFTER - BRANCH_T_BEFORE ))
echo "[run] daemon pause_ms = ${DAEMON_PAUSE_MS:-n/a}, wall-clock around BRANCH = ${WALL_BRANCH_MS} ms"

# ---- wait for agent to finish ----------------------------------------------
wait "$EXEC_PID" || true
jq -r '.stdout' "$OUT_DIR/exec.json" > "$OUT_DIR/agent.jsonl"

# ---- cleanup source sandbox ------------------------------------------------
echo "[run] tearing down source sandbox"
curl -fsS -X DELETE -H "Authorization: Bearer $FORKD_TOKEN" \
  "$FORKD_URL/v1/sandboxes/$SOURCE_ID" || true

# ---- analyze ---------------------------------------------------------------
echo "[run] analyzing"
"$PYTHON_BIN" "$HERE/analyze.py" \
  --agent-log "$OUT_DIR/agent.jsonl" \
  --server-log "$OUT_DIR/server.jsonl" \
  --daemon-pause-ms "${DAEMON_PAUSE_MS:-0}" \
  --baseline-interval-ms "$INTERVAL_MS" \
  --out-json "$OUT_DIR/report.json" \
  --out-md "$OUT_DIR/report.md"

echo "[run] done."
echo
cat "$OUT_DIR/report.md"
