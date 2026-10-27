#!/usr/bin/env bash
# bench-spawn-100.sh — spawn N parallel sandboxes that import numpy
# and return. Records wall-clock for each backend.
#
# Backends:
#   sandstorm       — pre-warmed parent with numpy in PID 1 + per-child netns
#   docker      — `docker run --runtime=runc sandstorm-numpy-bench python -c ...`
#   gvisor      — `docker run --runtime=runsc ...` (same image, gVisor isolation)
#   firecracker — N cold-boot microVMs (no snapshot, no agent)
#
# Output: JSON to /tmp/sandstorm-bench-results.json suitable for generate_charts.py.

set -uo pipefail

N="${N:-100}"
IMAGE="${IMAGE:-sandstorm-numpy-bench}"
WORK_FC="${WORK_FC:-$HOME/work/fc-quickstart}"
TAG="${TAG:-pyagent}"
OUT="${OUT:-/tmp/sandstorm-bench-results.json}"

say() { printf "\033[1;34m==>\033[0m %s\n" "$*"; }

# Ensure the numpy image exists.
if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
    say "building $IMAGE..."
    (
        echo "FROM python:3.12-slim"
        echo "RUN pip install --no-cache-dir numpy"
    ) | docker build -q -t "$IMAGE" - >/dev/null
fi

ms() { awk "BEGIN { printf \"%.0f\", ($2 - $1)*1000 }"; }

results=()

# -----------------------------------------------------------------------
# Docker (runc)
# -----------------------------------------------------------------------
say "docker run --runtime=runc × $N parallel..."
t0=$(date +%s.%N)
pids=()
for i in $(seq 1 "$N"); do
    docker run --runtime=runc --rm "$IMAGE" python -c \
        "import numpy; numpy.zeros(5).sum()" >/dev/null 2>&1 &
    pids+=($!)
done
for p in "${pids[@]}"; do wait "$p"; done
t1=$(date +%s.%N)
docker_ms=$(ms "$t0" "$t1")
echo "  docker (runc):   ${docker_ms} ms"
results+=("{\"backend\":\"docker\",\"n\":$N,\"total_ms\":$docker_ms}")

# -----------------------------------------------------------------------
# gVisor (runsc)
# -----------------------------------------------------------------------
if docker info 2>/dev/null | grep -q "runsc"; then
    say "docker run --runtime=runsc × $N parallel..."
    t0=$(date +%s.%N)
    pids=()
    for i in $(seq 1 "$N"); do
        docker run --runtime=runsc --rm "$IMAGE" python -c \
            "import numpy; numpy.zeros(5).sum()" >/dev/null 2>&1 &
        pids+=($!)
    done
    for p in "${pids[@]}"; do wait "$p"; done
    t1=$(date +%s.%N)
    gvisor_ms=$(ms "$t0" "$t1")
    echo "  gvisor (runsc):  ${gvisor_ms} ms"
    results+=("{\"backend\":\"gvisor\",\"n\":$N,\"total_ms\":$gvisor_ms}")
fi

# -----------------------------------------------------------------------
# sandstorm (per-child netns)
# -----------------------------------------------------------------------
if command -v sandstorm >/dev/null; then
    say "sandstorm fork --tag $TAG -n $N --per-child-netns..."
    OUT_LINE=$(sandstorm fork --tag "$TAG" -n "$N" --settle-secs 5 --per-child-netns 2>&1 | grep "total wall-clock")
    sandstorm_ms=$(echo "$OUT_LINE" | awk '{print $4}')
    echo "  sandstorm:           ${sandstorm_ms} ms"
    results+=("{\"backend\":\"sandstorm\",\"n\":$N,\"total_ms\":$sandstorm_ms}")
fi

# -----------------------------------------------------------------------
# Firecracker cold-boot (no snapshot)
# -----------------------------------------------------------------------
KERNEL="$WORK_FC/vmlinux-6.1.141"
ROOTFS="$WORK_FC/ubuntu-24.04.squashfs"
if [ -f "$KERNEL" ] && [ -f "$ROOTFS" ] && command -v firecracker >/dev/null; then
    say "firecracker cold-boot × $N parallel..."
    FCWORK=/tmp/sandstorm-bench-fc
    mkdir -p "$FCWORK"
    rm -f "$FCWORK"/*.sock "$FCWORK"/*.console

    t0=$(date +%s.%N)
    pids=()
    for i in $(seq 1 "$N"); do
        sock="$FCWORK/fc-$i.sock"
        console="$FCWORK/fc-$i.console"
        firecracker --api-sock "$sock" </dev/null >"$console" 2>&1 &
        pids+=($!)
    done
    for i in $(seq 1 "$N"); do
        sock="$FCWORK/fc-$i.sock"
        for _ in $(seq 1 60); do [ -S "$sock" ] && break; sleep 0.05; done
    done
    config_pids=()
    for i in $(seq 1 "$N"); do
        sock="$FCWORK/fc-$i.sock"
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
    for p in "${config_pids[@]}"; do wait "$p"; done
    t1=$(date +%s.%N)
    fc_ms=$(ms "$t0" "$t1")
    echo "  firecracker:     ${fc_ms} ms"
    results+=("{\"backend\":\"firecracker\",\"n\":$N,\"total_ms\":$fc_ms}")

    for p in "${pids[@]}"; do kill "$p" 2>/dev/null; done
fi

# -----------------------------------------------------------------------
# Emit JSON
# -----------------------------------------------------------------------
printf '[%s]\n' "$(IFS=,; echo "${results[*]}")" > "$OUT"
say "wrote $OUT"
cat "$OUT"
