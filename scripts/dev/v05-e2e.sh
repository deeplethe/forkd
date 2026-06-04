#!/bin/bash
# v0.5 end-to-end smoke test — diff snapshot chains, top to bottom.
#
# Exercises every user-facing surface added in v0.5 against a live
# forkd-controller + Firecracker on the localhost daemon. Designed
# to be re-runnable: cleans up before AND after.
#
# Sub-tests (11 in total):
#   Phase 2b  — snapshot-diff builds L1, then L2 (depth-2 chain)
#   Phase 4   — snapshot-info reports correct depth / parent / ancestors
#   Phase 4   — `rmi` on a chained parent returns HTTP 409
#   Phase 2a  — spawn from chain head + probe injected SIGNATURE
#   Phase 4   — snapshot-compact flattens to depth-0 (parent_tag=None)
#   Phase 3   — pack v2 emits 3 chain links in manifest
#   Phase 3   — unpack materializes all 3 link dirs
#   Phase 2a  — post-unpack spawn from chain head still probes correctly
#
# Usage:
#   FORKD_BIN=$(which forkd) \
#   FORKD_URL=http://127.0.0.1:8889 \
#   FORKD_TOKEN=<bearer-token> \
#   FORKD_BASE_TAG=demo-pyt \
#     ./scripts/dev/v05-e2e.sh
#
# Defaults:
#   FORKD_BIN       = `which forkd` (falls back to ./target/release/forkd)
#   FORKD_URL       = http://127.0.0.1:8889
#   FORKD_TOKEN     = read from FORKD_TOKEN_FILE if unset; required
#   FORKD_BASE_TAG  = demo-pyt — must be a registered base snapshot
#                     (python:3.12-slim or similar — must have python3
#                     in $PATH inside the guest)
#
# Exit code: number of FAILED sub-tests (0 = all green).

set -u

# ---------------- config ----------------
FORKD_BIN="${FORKD_BIN:-$(command -v forkd 2>/dev/null || echo ./target/release/forkd)}"
FORKD_URL="${FORKD_URL:-http://127.0.0.1:8889}"
FORKD_TOKEN_FILE="${FORKD_TOKEN_FILE:-/tmp/bench-pause/token}"
FORKD_BASE_TAG="${FORKD_BASE_TAG:-demo-pyt}"

if [ -z "${FORKD_TOKEN:-}" ]; then
    if [ -r "$FORKD_TOKEN_FILE" ]; then
        FORKD_TOKEN=$(cat "$FORKD_TOKEN_FILE")
    else
        echo "ERROR: FORKD_TOKEN not set and FORKD_TOKEN_FILE=$FORKD_TOKEN_FILE not readable" >&2
        exit 2
    fi
fi
export FORKD_URL FORKD_TOKEN

if [ ! -x "$FORKD_BIN" ]; then
    echo "ERROR: forkd binary not found / not executable: $FORKD_BIN" >&2
    echo "Set FORKD_BIN or run 'cargo build --release -p forkd-cli' first." >&2
    exit 2
fi

# ---------------- helpers ----------------
PASS=0
FAIL=0

curl_auth() {
    curl -sS -H "Authorization: Bearer $FORKD_TOKEN" "$@"
}

curl_json() {
    curl_auth -H "Content-Type: application/json" "$@"
}

run_test() {
    # Wraps a command, captures its exit code into PASS/FAIL counters.
    local name="$1"
    shift
    echo "▶ $name"
    if "$@"; then
        echo "  ✓ pass"
        PASS=$((PASS + 1))
    else
        local rc=$?
        echo "  ✗ FAIL (rc=$rc)"
        FAIL=$((FAIL + 1))
    fi
}

assert_eq() {
    # Light assertion helper for chained pipelines.
    local desc="$1" expected="$2" actual="$3"
    if [ "$expected" = "$actual" ]; then
        echo "  ✓ pass ($desc = $actual)"
        PASS=$((PASS + 1))
    else
        echo "  ✗ FAIL: $desc — expected '$expected' got '$actual'"
        FAIL=$((FAIL + 1))
    fi
}

# ---------------- pre-flight ----------------
echo "=== pre-flight ==="
if ! curl_auth --max-time 3 "$FORKD_URL/healthz" 2>/dev/null | grep -q '"ok":true'; then
    echo "ERROR: daemon at $FORKD_URL not reachable / unhealthy" >&2
    exit 2
fi
if ! "$FORKD_BIN" snapshot-info "$FORKD_BASE_TAG" >/dev/null 2>&1; then
    echo "ERROR: base snapshot tag '$FORKD_BASE_TAG' not registered with the daemon" >&2
    echo "Build one first (e.g. 'forkd from-image python:3.12-slim --tag demo-pyt')" >&2
    exit 2
fi
echo "  daemon: OK at $FORKD_URL"
echo "  base:   $FORKD_BASE_TAG"
echo "  forkd:  $FORKD_BIN"

# ---------------- cleanup leftovers ----------------
cleanup_e2e_tags() {
    # Leaves-first explicit deletes — robust to partial state from a
    # crashed prior run (e.g. e2e-l2 created without e2e-l1, or
    # e2e-flat from snapshot-compact still hanging around). `--force`
    # avoids 409 when one of these has acquired dependents from a
    # parallel run; we don't care about chain integrity at cleanup
    # time, just want a clean slate.
    for tag in e2e-l2 e2e-l1 e2e-flat; do
        "$FORKD_BIN" rmi "$tag" --force >/dev/null 2>&1
    done
    rm -f /tmp/v05-e2e-bundle.tar.zst
}
cleanup_e2e_tags
trap cleanup_e2e_tags EXIT

# Kill any orphan FC processes from prior runs.
sudo pkill -9 firecracker >/dev/null 2>&1 || true
sleep 1

# ---------------- Phase 2b: build chain ----------------
echo
echo "=== Phase 2b: build chain via snapshot-diff ==="
run_test "L1 build" "$FORKD_BIN" snapshot-diff \
    --from "$FORKD_BASE_TAG" --tag e2e-l1 \
    --exec "sh -c 'mkdir -p /tmp/agent && printf \"SIGNATURE=\\\"e2e-l1\\\"\\n\" > /tmp/agent/lib.py'" \
    --exec-timeout-secs 60

run_test "L2 build (chained off L1)" "$FORKD_BIN" snapshot-diff \
    --from e2e-l1 --tag e2e-l2 \
    --exec "sh -c 'printf \"SIGNATURE=\\\"e2e-l2\\\"\\n\" > /tmp/agent/lib.py'" \
    --exec-timeout-secs 60

# ---------------- Phase 4: snapshot-info ----------------
echo
echo "=== Phase 4: snapshot-info reports chain shape ==="
INFO_L2=$("$FORKD_BIN" snapshot-info e2e-l2 --json)
DEPTH=$(echo    "$INFO_L2" | python3 -c 'import json,sys;print(json.load(sys.stdin)["chain_depth"])')
PARENT=$(echo   "$INFO_L2" | python3 -c 'import json,sys;print(json.load(sys.stdin)["parent_tag"])')
ANCESTORS=$(echo "$INFO_L2" | python3 -c 'import json,sys;print(",".join(json.load(sys.stdin)["ancestors"]))')
assert_eq "depth"     "2"                       "$DEPTH"
assert_eq "parent"    "e2e-l1"                  "$PARENT"
assert_eq "ancestors" "$FORKD_BASE_TAG,e2e-l1"  "$ANCESTORS"

# ---------------- Phase 4: rmi safety ----------------
echo
echo "=== Phase 4: rmi without cascade returns 409 ==="
RMI_OUT=$("$FORKD_BIN" rmi e2e-l1 2>&1 || true)
if echo "$RMI_OUT" | grep -q "HTTP 409"; then
    echo "  ✓ pass (refused with 409 — daemon protected the chain parent)"
    PASS=$((PASS + 1))
else
    echo "  ✗ FAIL: $RMI_OUT"
    FAIL=$((FAIL + 1))
fi

# ---------------- Phase 2a: spawn from chain head + probe ----------------
spawn_and_probe() {
    local head_tag="$1" expected_sig="$2"
    local spawn_resp sb probe stdout
    spawn_resp=$(curl_json -d "{\"snapshot_tag\":\"$head_tag\",\"n\":1}" "$FORKD_URL/v1/sandboxes")
    sb=$(echo "$spawn_resp" | python3 -c 'import json,sys;d=json.load(sys.stdin);print(d[0]["id"] if isinstance(d,list) else "")')
    if [ -z "$sb" ]; then
        echo "  ✗ FAIL spawn: $spawn_resp"
        FAIL=$((FAIL + 1))
        return 1
    fi
    sleep 1
    probe=$(curl_json -X POST -d \
        '{"args":["sh","-c","python3 -c \"import sys;sys.path.insert(0,\\\"/tmp/agent\\\");import lib;print(lib.SIGNATURE)\""],"timeout_secs":15}' \
        "$FORKD_URL/v1/sandboxes/$sb/exec")
    stdout=$(echo "$probe" | python3 -c 'import json,sys;print(json.load(sys.stdin).get("stdout","").strip())')
    curl_auth -X DELETE "$FORKD_URL/v1/sandboxes/$sb" -o /dev/null
    assert_eq "SIGNATURE from $head_tag" "$expected_sig" "$stdout"
}

echo
echo "=== Phase 2a: spawn from chain head + verify probe ==="
spawn_and_probe e2e-l2 e2e-l2

# ---------------- Phase 4: snapshot-compact ----------------
echo
echo "=== Phase 4: snapshot-compact flattens to depth=0 ==="
run_test "compact e2e-l2 → e2e-flat" "$FORKD_BIN" snapshot-compact --from e2e-l2 --to e2e-flat
FLAT_DEPTH=$("$FORKD_BIN" snapshot-info e2e-flat --json | python3 -c 'import json,sys;print(json.load(sys.stdin)["chain_depth"])')
FLAT_PARENT=$("$FORKD_BIN" snapshot-info e2e-flat --json | python3 -c 'import json,sys;d=json.load(sys.stdin);print(d.get("parent_tag") or "")')
assert_eq "compact depth"      "0" "$FLAT_DEPTH"
assert_eq "compact parent_tag" ""  "$FLAT_PARENT"

# ---------------- Phase 3: pack v2 then unpack restore ----------------
echo
echo "=== Phase 3: pack v2 → rmi cascade → unpack → re-spawn ==="
run_test "pack e2e-l2 (v2)" "$FORKD_BIN" pack \
    --tag e2e-l2 --out /tmp/v05-e2e-bundle.tar.zst

# Inspect the manifest before destroying the chain.
PACK_VER=$(tar -I zstd -xOf /tmp/v05-e2e-bundle.tar.zst manifest.toml | awk '/^forkd_pack_version/ {print $3}')
CHAIN_LINKS=$(tar -I zstd -xOf /tmp/v05-e2e-bundle.tar.zst manifest.toml | grep -c '^\[\[chain\]\]' || true)
assert_eq "manifest version" "2" "$PACK_VER"
assert_eq "chain link count" "3" "$CHAIN_LINKS"

# Cascade-delete the chain we just packed, then unpack with --force
# so it can overwrite the still-existing base snapshot. We intentionally
# do NOT pre-`rmi` the base — if `unpack` fails partway through, this
# host still has its working base snapshot to recover with.
"$FORKD_BIN" rmi e2e-l1 --cascade >/dev/null

run_test "unpack v2 chain bundle" "$FORKD_BIN" unpack /tmp/v05-e2e-bundle.tar.zst --force

echo
echo "=== End-to-end: spawn from post-unpack chain head ==="
spawn_and_probe e2e-l2 e2e-l2

# ---------------- summary ----------------
echo
echo "=================================="
echo " v0.5 E2E: $PASS passed, $FAIL failed"
echo "=================================="
exit $FAIL
