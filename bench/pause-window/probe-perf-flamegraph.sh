#!/usr/bin/env bash
# probe-perf-flamegraph.sh — perf record + flamegraph the FC snapshot
# worker during a slow BRANCH. Identifies the per-snapshot growing
# loop hypothesized in PROBE-multi-branch-anomaly.md.
#
# Requires:
#   - FC binary at /usr/local/bin/firecracker built with DWARF +
#     force-frame-pointers (see PROBE-multi-branch-anomaly.md for
#     the build recipe)
#   - perf >= 5.x with --call-graph dwarf
#   - FlameGraph repo at $FLAMEGRAPH_DIR
set -euo pipefail

FORKD_URL=${FORKD_URL:-http://127.0.0.1:8889}
FORKD_TOKEN=${FORKD_TOKEN:-$(cat "${FORKD_TOKEN_FILE:-/etc/forkd/token}" 2>/dev/null || echo "")}
TAG=${TAG:-coding-agent-fork-prewarm-v1}
WARMUP_BRANCHES=${WARMUP_BRANCHES:-6}
GAP_SECS=${GAP_SECS:-3}
FLAMEGRAPH_DIR=${FLAMEGRAPH_DIR:-/home/yangdongxu/work/FlameGraph}
OUT_BASE="/tmp/fc-perf-$(date +%s)"
OUT_DATA="$OUT_BASE.data"
OUT_FOLDED="$OUT_BASE.folded"
OUT_SVG="$OUT_BASE.svg"
auth=(-H "Authorization: Bearer $FORKD_TOKEN")

echo "[probe] outputs: $OUT_DATA / $OUT_FOLDED / $OUT_SVG" >&2

# Spawn source
spawn=$(curl -fsS "${auth[@]}" -H "Content-Type: application/json" \
  -d "{\"snapshot_tag\":\"$TAG\",\"n\":1,\"per_child_netns\":true}" \
  "$FORKD_URL/v1/sandboxes")
sb_id=$(echo "$spawn" | jq -r '.[0].id')
fc_pid=$(echo "$spawn" | jq -r '.[0].pid')
echo "[probe] sandbox=$sb_id fc_pid=$fc_pid" >&2
sleep 2

# Warmup into the slow regime
for i in $(seq 1 "$WARMUP_BRANCHES"); do
  sleep "$GAP_SECS"
  resp=$(curl -fsS "${auth[@]}" -H "Content-Type: application/json" \
    -d "{\"diff\":true}" \
    "$FORKD_URL/v1/sandboxes/$sb_id/branch")
  pause=$(echo "$resp" | jq -r '.pause_ms')
  echo "[probe] warmup BRANCH $i: pause_ms=$pause" >&2
done

# Start perf record with DWARF call-graph
echo "[probe] starting perf record on pid $fc_pid (DWARF unwinding, 10s window)" >&2
# -a captures all CPUs system-wide; we filter to FC's pid in post-processing.
# Required because per-pid `-p` consistently misses the brief on-CPU
# windows during a slow BRANCH (FC is off-CPU ~94% of pause).
sudo perf record -F 99 -a -g --call-graph fp \
  -o "$OUT_DATA" -- sleep 10 &
perf_pid=$!
sleep 0.5

# Fire 1-2 slow BRANCHes inside the perf window
sleep "$GAP_SECS"
echo "[probe] firing profiled BRANCH #1" >&2
resp=$(curl -fsS "${auth[@]}" -H "Content-Type: application/json" \
  -d "{\"diff\":true}" \
  "$FORKD_URL/v1/sandboxes/$sb_id/branch")
echo "[probe] profiled #1: pause_ms=$(echo "$resp" | jq -r .pause_ms)" >&2

sleep 1
echo "[probe] firing profiled BRANCH #2" >&2
resp=$(curl -fsS "${auth[@]}" -H "Content-Type: application/json" \
  -d "{\"diff\":true}" \
  "$FORKD_URL/v1/sandboxes/$sb_id/branch")
echo "[probe] profiled #2: pause_ms=$(echo "$resp" | jq -r .pause_ms)" >&2

# Wait for perf to finish its sleep window
wait "$perf_pid" 2>/dev/null || true

# Cleanup
curl -fsS -X DELETE "${auth[@]}" "$FORKD_URL/v1/sandboxes/$sb_id" > /dev/null || true

# Convert perf.data → folded stacks → SVG
echo "" >&2
echo "[probe] converting perf.data → folded stacks → SVG" >&2
sudo chown "$USER:$USER" "$OUT_DATA"
# Filter to FC's pid since we captured system-wide.
sudo perf script -i "$OUT_DATA" --pid "$fc_pid" 2>/dev/null \
  | "$FLAMEGRAPH_DIR/stackcollapse-perf.pl" > "$OUT_FOLDED"
"$FLAMEGRAPH_DIR/flamegraph.pl" --title "FC snapshot worker (BRANCH slow regime)" \
  "$OUT_FOLDED" > "$OUT_SVG"

echo "" >&2
echo "===== top folded stacks (CPU samples; bigger = hotter) =====" >&2
sort -t' ' -k2 -n -r "$OUT_FOLDED" | head -15 >&2

echo "" >&2
echo "[probe] flamegraph at $OUT_SVG" >&2
echo "[probe] folded data at $OUT_FOLDED" >&2
echo "[probe] raw perf data at $OUT_DATA" >&2
