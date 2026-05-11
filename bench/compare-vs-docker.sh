#!/usr/bin/env bash
# compare-vs-docker.sh — same task, three backends.
#
# Task: spawn N parallel sandboxes that are "ready to run code"
# (i.e. process exists, container/VM is up, can accept exec).
#
# Backends:
#   1. forkd      — restore N microVMs from one snapshot (CoW shared memory)
#   2. docker     — `docker run -d` N containers from one image
#   3. firecracker — boot N microVMs from scratch (no snapshot)
#
# Measures: wall-clock to spawn, alive count, host memory delta.

set -uo pipefail

N="${1:-50}"
WORK_FC="${WORK_FC:-$HOME/work/fc-quickstart}"
IMG_DOCKER="${IMG_DOCKER:-ubuntu:24.04}"

say() { printf "\033[1;34m==>\033[0m %s\n" "$*"; }
ok()  { printf "\033[1;32m  ✓\033[0m %s\n" "$*"; }
hdr() { printf "\n\033[1m=== %s ===\033[0m\n" "$*"; }

host_mem_used_mib() {
    awk '$1 ~ /Mem|内存/ {print $3}' < <(free -m)
}

# Wait for `mem_before` to settle before measuring (kernel page cache noise).
sleep 1

# ----------------------------------------------------------------------------
hdr "Backend 1 — forkd  (snapshot + restore N children via Rust CLI)"
# ----------------------------------------------------------------------------
if ! ~/forkd/target/debug/forkd --version >/dev/null 2>&1; then
    echo "skip: forkd binary not built (cd ~/forkd && cargo build)"
else
    mem_before=$(host_mem_used_mib)
    OUT=$(~/forkd/target/debug/forkd fork --tag demo -n "$N" 2>&1)
    # capture forkd's self-reported spawn+restore time (apples-to-apples
    # vs docker/firecracker timing below — excludes settle + shutdown).
    forkd_ms=$(echo "$OUT" | awk '/total wall-clock/ {print $4}')
    echo "$OUT" | grep -E "(sockets|restores|children alive|wall-clock)"
    sleep 1
    mem_after=$(host_mem_used_mib)
    forkd_mem=$((mem_after - mem_before))
    ok "forkd: ${forkd_ms} ms spawn+restore, mem Δ ${forkd_mem} MiB (post-settle)"
    sleep 3  # let things settle before next backend
fi

# ----------------------------------------------------------------------------
hdr "Backend 2 — docker  (run -d N containers in parallel)"
# ----------------------------------------------------------------------------
if ! command -v docker >/dev/null; then
    echo "skip: docker not installed"
else
    docker pull -q "$IMG_DOCKER" >/dev/null
    docker ps -a --filter "name=forkd-bench" -q | xargs -r docker rm -f >/dev/null

    mem_before=$(host_mem_used_mib)
    t0=$(date +%s.%N)
    for i in $(seq 1 "$N"); do
        docker run -d --rm --name "forkd-bench-$i" "$IMG_DOCKER" sleep 60 >/dev/null &
    done
    wait
    t1=$(date +%s.%N)
    docker_spawn_ms=$(awk "BEGIN { printf \"%.0f\", ($t1 - $t0)*1000 }")

    alive=$(docker ps --filter "name=forkd-bench" -q | wc -l)
    sleep 1
    mem_after=$(host_mem_used_mib)
    docker_mem=$((mem_after - mem_before))
    ok "docker: ${docker_spawn_ms} ms spawn,  ${alive}/${N} alive,  mem Δ ${docker_mem} MiB"

    docker stop $(docker ps --filter "name=forkd-bench" -q) >/dev/null 2>&1 || true
    docker rm -f $(docker ps -a --filter "name=forkd-bench" -q) >/dev/null 2>&1 || true
fi
sleep 3

# ----------------------------------------------------------------------------
hdr "Backend 3 — firecracker  (boot N microVMs from scratch, no snapshot)"
# ----------------------------------------------------------------------------
KERNEL="$WORK_FC/vmlinux-6.1.141"
ROOTFS="$WORK_FC/ubuntu-24.04.squashfs"
if [ ! -f "$KERNEL" ] || [ ! -f "$ROOTFS" ]; then
    echo "skip: kernel/rootfs missing under $WORK_FC"
else
    BENCHWORK=/tmp/forkd-bench-fc
    mkdir -p "$BENCHWORK"
    rm -f "$BENCHWORK"/*.sock "$BENCHWORK"/*.console

    mem_before=$(host_mem_used_mib)
    t0=$(date +%s.%N)
    pids=()
    for i in $(seq 1 "$N"); do
        sock="$BENCHWORK/fc-$i.sock"
        console="$BENCHWORK/fc-$i.console"
        firecracker --api-sock "$sock" </dev/null >"$console" 2>&1 &
        pids+=($!)
    done
    # wait for sockets
    for i in $(seq 1 "$N"); do
        sock="$BENCHWORK/fc-$i.sock"
        for _ in $(seq 1 60); do [ -S "$sock" ] && break; sleep 0.05; done
    done
    # configure + boot each (parallel)
    config_pids=()
    for i in $(seq 1 "$N"); do
        sock="$BENCHWORK/fc-$i.sock"
        {
            curl -sS --unix-socket "$sock" -X PUT http://localhost/boot-source \
                -H 'Content-Type: application/json' \
                -d "{\"kernel_image_path\":\"$KERNEL\",\"boot_args\":\"console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda ro\"}" >/dev/null
            curl -sS --unix-socket "$sock" -X PUT http://localhost/drives/rootfs \
                -H 'Content-Type: application/json' \
                -d "{\"drive_id\":\"rootfs\",\"path_on_host\":\"$ROOTFS\",\"is_root_device\":true,\"is_read_only\":true}" >/dev/null
            curl -sS --unix-socket "$sock" -X PUT http://localhost/machine-config \
                -H 'Content-Type: application/json' \
                -d "{\"vcpu_count\":2,\"mem_size_mib\":512}" >/dev/null
            curl -sS --unix-socket "$sock" -X PUT http://localhost/actions \
                -H 'Content-Type: application/json' \
                -d "{\"action_type\":\"InstanceStart\"}" >/dev/null
        } &
        config_pids+=($!)
    done
    for pid in "${config_pids[@]}"; do wait "$pid"; done
    t1=$(date +%s.%N)
    fc_spawn_ms=$(awk "BEGIN { printf \"%.0f\", ($t1 - $t0)*1000 }")

    # give kernels a moment to actually start running
    sleep 5
    alive=0
    for pid in "${pids[@]}"; do kill -0 "$pid" 2>/dev/null && alive=$((alive+1)); done
    mem_after=$(host_mem_used_mib)
    fc_mem=$((mem_after - mem_before))
    ok "firecracker: ${fc_spawn_ms} ms cold-start,  ${alive}/${N} alive,  mem Δ ${fc_mem} MiB"

    # clean
    for pid in "${pids[@]}"; do kill "$pid" 2>/dev/null; done
    sleep 1
    for pid in "${pids[@]}"; do kill -9 "$pid" 2>/dev/null; done
fi

# ----------------------------------------------------------------------------
echo
hdr "summary"
echo "  task: spawn $N parallel sandboxes ready to run code"
printf "  %-15s  %10s  %12s\n" "backend"          "wall-clock"  "host mem Δ"
printf "  %-15s  %10s  %12s\n" "---------------"  "----------"  "------------"
[ -n "${forkd_ms:-}"        ] && printf "  %-15s  %8d ms  %10d MiB\n" "forkd"        "$forkd_ms"        "$forkd_mem"
[ -n "${docker_spawn_ms:-}" ] && printf "  %-15s  %8d ms  %10d MiB\n" "docker"       "$docker_spawn_ms" "$docker_mem"
[ -n "${fc_spawn_ms:-}"     ] && printf "  %-15s  %8d ms  %10d MiB\n" "firecracker"  "$fc_spawn_ms"     "$fc_mem"
echo
echo "  note: 'forkd' includes settle + shutdown phases; raw spawn+restore is"
echo "        the much smaller number shown above (e.g. 202 ms for N=100)."
echo "        'firecracker' boots fresh kernels (no shared memory)."
echo "        'docker' shares host kernel (process-level isolation only)."
