#!/usr/bin/env bash
# Verify the FC mem_backend.shared patch by checking /proc/PID/maps.
#
# Expected:
#   shared=false -> rw-p (MAP_PRIVATE, unchanged FC behavior)
#   shared=true  -> rw-s (MAP_SHARED,  what the patch enables)

set -euo pipefail

FC_BIN="${FC_BIN:-$HOME/firecracker/build/cargo_target/release/firecracker}"
SNAP_DIR="${SNAP_DIR:-$HOME/.local/share/forkd/snapshots/coding-agent-fork-prewarm-v1}"

cleanup_pids=()
trap 'for p in "${cleanup_pids[@]}"; do sudo kill -9 "$p" 2>/dev/null || true; done' EXIT

run_one() {
    local shared=$1
    local label=$2
    local workdir
    workdir=$(mktemp -d -t fc-shared-test-XXXX)
    local sock="$workdir/api.sock"
    local logfile="$workdir/fc.log"

    echo "=== run with shared=$shared ($label) ==="
    sudo touch "$logfile"
    sudo "$FC_BIN" --api-sock "$sock" --log-path "$logfile" --level Debug >"$workdir/stdout.log" 2>&1 &
    local sudo_pid=$!
    cleanup_pids+=("$sudo_pid")
    sleep 0.5

    # Resolve the REAL firecracker PID (sudo's direct child)
    local fc_pid
    fc_pid=$(pgrep -P "$sudo_pid" || true)
    if [[ -z "$fc_pid" ]]; then
        echo "  could not resolve real FC PID under sudo $sudo_pid"
        cat "$workdir/stdout.log"
        return 1
    fi
    cleanup_pids+=("$fc_pid")
    echo "  FC pid = $fc_pid (sudo wrapper $sudo_pid)"

    local body
    body=$(cat <<JSON
{
  "snapshot_path": "$SNAP_DIR/vmstate",
  "mem_backend": {
    "backend_path": "$SNAP_DIR/memory.bin",
    "backend_type": "File",
    "shared": $shared
  },
  "enable_diff_snapshots": false,
  "resume_vm": false
}
JSON
)
    local http_status
    http_status=$(sudo curl --unix-socket "$sock" -s -o "$workdir/curl.out" -w "%{http_code}"         -X PUT "http://localhost/snapshot/load"         -H "Content-Type: application/json"         -d "$body" || echo curl_failed)
    echo "  /snapshot/load -> HTTP $http_status"
    if [[ "$http_status" != "204" && "$http_status" != "200" ]]; then
        echo "  curl body: $(sudo cat "$workdir/curl.out")"
        return 1
    fi

    echo "  /proc/$fc_pid/maps lines matching memory.bin:"
    sudo grep memory.bin "/proc/$fc_pid/maps" | head -3 | sed 's/^/    /'
    local perms
    perms=$(sudo grep memory.bin "/proc/$fc_pid/maps" | head -1 | awk '{print $2}')
    local share_flag="${perms:3:1}"
    echo "  perms column = $perms   (share_flag = '$share_flag')"

    # Clean up immediately so we do not pile up FCs across runs
    sudo kill -9 "$fc_pid" 2>/dev/null || true
    sudo kill -9 "$sudo_pid" 2>/dev/null || true
    wait "$sudo_pid" 2>/dev/null || true

    if [[ "$shared" == "false" && "$share_flag" == "p" ]]; then
        echo "  OK shared=false -> MAP_PRIVATE (expected)"
        return 0
    elif [[ "$shared" == "true" && "$share_flag" == "s" ]]; then
        echo "  OK shared=true  -> MAP_SHARED  (PATCH WORKS)"
        return 0
    else
        echo "  FAIL: unexpected share_flag '$share_flag' for shared=$shared"
        return 2
    fi
}

run_one false "stock-compatible default"
echo
run_one true "patched behavior"

echo
echo "PATCH VERIFIED"
