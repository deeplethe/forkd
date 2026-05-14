#!/usr/bin/env bash
# Re-bench CubeSandbox with the same 2 Gi writable-layer template the
# original run used, but at smaller N (N=1, N=5, N=10) so we stay
# under the 30 GiB host RAM budget (template spec = 2 GiB per sandbox).
#
# Uses cube-api's E2B-compatible REST endpoints directly so we don't
# need to chase the right req.json shape for cubemastercli multirun.
set -euo pipefail

API=http://127.0.0.1:6000
KEY="X-API-Key: local"

create_one() {
    local out
    out=$(curl -sS -X POST "$API/sandboxes" -H "Content-Type: application/json" -H "$KEY" \
                -d '{"templateID":"forkd-bench-pynp"}' 2>&1)
    # sandboxID extraction without jq
    echo "$out" | sed -n 's/.*"sandboxID":"\([^"]*\)".*/\1/p'
}

kill_one() {
    local id="$1"
    curl -sS -X DELETE "$API/sandboxes/$id" -H "$KEY" > /dev/null 2>&1 || true
}

run_n() {
    local N="$1"
    echo "==> N=$N concurrent create"
    declare -a pids ids
    local tmpd
    tmpd=$(mktemp -d)
    local t0=$(date +%s%N)
    for i in $(seq 1 "$N"); do
        ( create_one > "$tmpd/$i" ) &
        pids[$i]=$!
    done
    for p in "${pids[@]}"; do wait "$p" 2>/dev/null || true; done
    local t1=$(date +%s%N)
    local elapsed_ms=$(( (t1 - t0) / 1000000 ))

    local succ=0
    for i in $(seq 1 "$N"); do
        id=$(cat "$tmpd/$i" 2>/dev/null)
        if [ -n "$id" ]; then
            ids+=("$id")
            succ=$((succ + 1))
        fi
    done
    echo "    succeeded: $succ / $N"
    echo "    wall-clock: ${elapsed_ms} ms"

    echo "    killing ..."
    for id in "${ids[@]:-}"; do
        [ -n "$id" ] && kill_one "$id" &
    done
    wait
    rm -rf "$tmpd"
    sleep 3
}

# warm up: cubelet pool may need a moment after restart
echo "==> warming cubelet (single sandbox, discarded)"
warm_id=$(create_one || true)
[ -n "$warm_id" ] && kill_one "$warm_id"
sleep 3

run_n 1
run_n 5
run_n 10
