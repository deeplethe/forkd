#!/usr/bin/env bash
# probe-offcpu.sh — off-CPU profile FC during slow BRANCH.
#
# Hooks the scheduler to record (kernel-stack, sleep-duration) pairs
# for FC's threads. The hottest stack is the kernel function FC is
# parked on — the actual bottleneck for the multi-BRANCH anomaly
# (#146). On-CPU probes consistently miss it because FC is off-CPU
# ~99.7 % of the slow window (PROBE-multi-branch-anomaly.md, "Follow-up:
# perf flamegraph" section).
set -euo pipefail

FORKD_URL=${FORKD_URL:-http://127.0.0.1:8889}
FORKD_TOKEN=${FORKD_TOKEN:-$(cat "${FORKD_TOKEN_FILE:-/etc/forkd/token}" 2>/dev/null || echo "")}
TAG=${TAG:-coding-agent-fork-prewarm-v1}
WARMUP_BRANCHES=${WARMUP_BRANCHES:-6}
GAP_SECS=${GAP_SECS:-3}
OUT="/tmp/fc-offcpu-$(date +%s).txt"
auth=(-H "Authorization: Bearer $FORKD_TOKEN")

echo "[probe] output → $OUT" >&2

# Spawn source
spawn=$(curl -fsS "${auth[@]}" -H "Content-Type: application/json" \
  -d "{\"snapshot_tag\":\"$TAG\",\"n\":1,\"per_child_netns\":true}" \
  "$FORKD_URL/v1/sandboxes")
sb_id=$(echo "$spawn" | jq -r '.[0].id')
fc_pid=$(echo "$spawn" | jq -r '.[0].pid')
echo "[probe] sandbox=$sb_id fc_pid=$fc_pid" >&2
sleep 2

# Warmup into slow regime
for i in $(seq 1 "$WARMUP_BRANCHES"); do
  sleep "$GAP_SECS"
  resp=$(curl -fsS "${auth[@]}" -H "Content-Type: application/json" \
    -d "{\"diff\":true}" \
    "$FORKD_URL/v1/sandboxes/$sb_id/branch")
  echo "[probe] warmup BRANCH $i: pause_ms=$(echo "$resp" | jq -r .pause_ms)" >&2
done

# bpftrace off-CPU probe.
#
# kprobe:finish_task_switch fires when a task is about to start running.
# arg0 is the previously-running task (prev_task_struct*); arg0->pid is
# its TID, arg0->tgid its PID. We filter to FC's tgid.
#
# At this point the CURRENT context is the task that's just resumed —
# i.e. NOT FC. We can read FC's kstack via kstack(perf, K) only if we
# walk prev->stack, which bpftrace doesn't expose directly. Workaround:
# capture stacks at the OUTGOING side via sched_switch tracepoint
# (current() is still prev there).
echo "[probe] starting bpftrace off-CPU probe (12s window)" >&2
sudo bpftrace -e "
tracepoint:sched:sched_switch
/ pid == $fc_pid /
{
    // current task is prev (going to sleep). Capture its stacks +
    // start time keyed by its TID (args->prev_pid).
    @sleep_start[args->prev_pid] = nsecs;
    @sleep_ustack[args->prev_pid] = ustack(perf, 16);
    @sleep_kstack[args->prev_pid] = kstack(perf, 16);
}

tracepoint:sched:sched_wakeup
/ args->pid != 0 && @sleep_start[args->pid] != 0 /
{
    \$dur_us = (nsecs - @sleep_start[args->pid]) / 1000;
    @offcpu_us[@sleep_kstack[args->pid]] = sum(\$dur_us);
    @offcpu_count[@sleep_kstack[args->pid]] = count();
    delete(@sleep_start[args->pid]);
    delete(@sleep_ustack[args->pid]);
    delete(@sleep_kstack[args->pid]);
}

interval:s:12 { exit(); }
" > "$OUT" 2>&1 &
bp_pid=$!
sleep 0.5

# Fire 2 slow BRANCHes inside the probe window
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

wait "$bp_pid" 2>/dev/null || true

# Cleanup
curl -fsS -X DELETE "${auth[@]}" "$FORKD_URL/v1/sandboxes/$sb_id" > /dev/null || true

echo "" >&2
echo "[probe] done. Top off-CPU kernel stacks (longest sleep total):" >&2
echo "" >&2
# bpftrace map output: @offcpu_us[\n stack \n]: N\n.
# Just dump the whole map ordered.
awk '
/^@offcpu_us\[/,/^[[:space:]]*[0-9]+$/ {
    print
}
' "$OUT" >&2 || true

echo "" >&2
echo "[probe] raw output: $OUT" >&2
