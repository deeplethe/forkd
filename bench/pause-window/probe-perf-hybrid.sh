#!/usr/bin/env bash
# probe-perf-hybrid.sh — perf with explicit hybrid (P-core + E-core)
# event sampling. Tests whether the previous round's "1 sample in FC"
# result was Alder Lake E-core blindness — `cpu_core/cycles/P` only
# samples on P-cores by default; if FC main thread ran on an E-core,
# perf would silently miss it.
set -euo pipefail

FORKD_URL=${FORKD_URL:-http://127.0.0.1:8889}
FORKD_TOKEN=${FORKD_TOKEN:-$(cat "${FORKD_TOKEN_FILE:-/etc/forkd/token}" 2>/dev/null || echo "")}
TAG=${TAG:-coding-agent-fork-prewarm-v1}
WARMUP_BRANCHES=${WARMUP_BRANCHES:-6}
GAP_SECS=${GAP_SECS:-3}
OUT_BASE="/tmp/fc-hybrid-$(date +%s)"
auth=(-H "Authorization: Bearer $FORKD_TOKEN")

echo "[probe] outputs base: $OUT_BASE" >&2

spawn=$(curl -fsS "${auth[@]}" -H "Content-Type: application/json" \
  -d "{\"snapshot_tag\":\"$TAG\",\"n\":1,\"per_child_netns\":true}" \
  "$FORKD_URL/v1/sandboxes")
sb_id=$(echo "$spawn" | jq -r '.[0].id')
fc_pid=$(echo "$spawn" | jq -r '.[0].pid')
echo "[probe] sandbox=$sb_id fc_pid=$fc_pid" >&2
sleep 2

for i in $(seq 1 "$WARMUP_BRANCHES"); do
  sleep "$GAP_SECS"
  resp=$(curl -fsS "${auth[@]}" -H "Content-Type: application/json" \
    -d "{\"diff\":true}" \
    "$FORKD_URL/v1/sandboxes/$sb_id/branch")
  echo "[probe] warmup BRANCH $i: pause_ms=$(echo "$resp" | jq -r .pause_ms)" >&2
done

# Explicit hybrid event sampling: P-core cycles + E-core cycles.
# Both are needed on Alder Lake; default perf record uses only
# cpu_core/cycles/P which misses E-core code.
echo "[probe] perf record (hybrid events, 10s window)" >&2
sudo perf record -F 99 -a -g --call-graph fp \
  -e cpu_core/cycles/P -e cpu_atom/cycles/P \
  -o "$OUT_BASE.data" -- sleep 10 &
perf_pid=$!
sleep 0.5

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

wait "$perf_pid" 2>/dev/null || true

curl -fsS -X DELETE "${auth[@]}" "$FORKD_URL/v1/sandboxes/$sb_id" > /dev/null || true

# Make readable + dump
sudo chmod 644 "$OUT_BASE.data"
echo "" >&2
echo "===== sample counts by event + process =====" >&2
sudo perf script -i "$OUT_BASE.data" 2>/dev/null | awk '{print $1}' | sort | uniq -c | sort -rn | head -10 >&2
echo "" >&2
echo "===== firecracker process samples (full count) =====" >&2
sudo perf script -i "$OUT_BASE.data" 2>/dev/null | grep -c "^firecracker" >&2
echo "" >&2
echo "===== firecracker on-CPU leaf functions =====" >&2
sudo perf script -i "$OUT_BASE.data" 2>/dev/null > "$OUT_BASE.script"
sudo chmod 644 "$OUT_BASE.script"
python3 - <<EOF
import re
samples = open("$OUT_BASE.script").read().split("\n\n")
samples = [s for s in samples if s.strip()]
fc = [s for s in samples if s.startswith("firecracker")]
print(f"  FC samples: {len(fc)}")
leaves = {}
for s in fc:
    lines = s.splitlines()
    if len(lines) < 2: continue
    leaf = lines[1].strip()
    m = re.match(r"[0-9a-f]+\s+([^+]+)", leaf)
    fn = m.group(1) if m else leaf[:80]
    leaves[fn] = leaves.get(fn, 0) + 1
for fn, c in sorted(leaves.items(), key=lambda x: -x[1])[:15]:
    print(f"  {c:4d}  {fn[:90]}")
EOF

echo "" >&2
echo "[probe] raw data: $OUT_BASE.data" >&2
echo "[probe] perf script: $OUT_BASE.script" >&2
