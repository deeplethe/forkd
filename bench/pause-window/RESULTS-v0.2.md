# Pause-window — first-cut results (forkd v0.2)

5 trials, single config, single host. This is the **methodology
validation** run that closes the v0.2-era pause-window question
and seeds the v0.3 userfaultfd paper §2.

## TL;DR

For a 513 MiB source VM running a TCP ping/pong agent (one
outstanding request, 100 ms cadence), branching pauses the source
for **4.26 s ± 0.41 s** (mean ± std across 5 trials, range
3.76–4.88 s). The cost lands in two distinct places:

1. **External observers** see the pause as-is — the host-side
   echo server sees a 4.4 s gap in echoed frames.
2. **Agents inside the guest see almost nothing.** Connection
   survival = 5/5, in-flight loss = 0/5, post-resume RTT p99
   returns to baseline (1–2 ms) within one round-trip.

The reason for (2) is interesting and goes into the paper. See
*"Why guest-internal agents are pause-blind"* below.

## Setup

- Host: yangdongxu-desktop, Ubuntu 24.04, Linux 6.14, 20 vCPU, 30 GiB RAM, KVM enabled
- forkd-controller built from commit `fc0a0d2`
  (`security: validate snapshot_tag in create_sandbox + harden defaults` and ancestors)
- Source rootfs: `python:3.12-slim` + `python3` (built via
  `scripts/build-rootfs.sh`)
- Source memory: 513 MiB (firecracker default for this rootfs)
- Snapshot storage: `~/.local/share/forkd/snapshots/` on local disk

Each trial: spawn one source sandbox, run `agent.py` for 30 s
sending a 16-byte frame every 100 ms to a host-side
`echo_server.py`, trigger `POST /v1/sandboxes/:id/branch` at
t = 10 s, collect logs.

## Numbers

| Trial | Read timeout | Daemon `pause_ms` | App-observed pause | In-flight lost | Connection survived | RTT p99 after pause |
|---|---|---:|---:|:---:|:---:|---:|
| r1 | 30 000 ms | 4053 ms | 4041 ms | 0 | ✅ | 1.42 ms |
| r2 | **1 000 ms** | 4328 ms | 4322 ms | 0 | ✅ | 2.00 ms |
| r3 | 30 000 ms | 3761 ms | 3742 ms | 0 | ✅ | 1.00 ms |
| r4 | 30 000 ms | 4286 ms | 4279 ms | 0 | ✅ | 1.00 ms |
| r5 | 30 000 ms | 4884 ms | 4862 ms | 0 | ✅ | 2.00 ms |

**Stats across all 5:**
- Daemon pause: **mean 4262 ms, std 414 ms, range 3761–4884 ms**
- App pause: **mean 4249 ms, std 413 ms, range 3742–4862 ms**
- App-vs-daemon overhead: **mean 13 ms** (well within jitter)

Reproducibility: trials run back-to-back from the same snapshot,
same rootfs, same daemon process. The ~10 % std comes from disk
write jitter (memory.bin gets a fresh 513 MiB write per branch).

Raw per-trial reports: `trial-{1,2-tight,3,4,5}.json` in this
directory.

## Why guest-internal agents are pause-blind

The most surprising finding: even with a 1 s socket read timeout
(`--read-timeout-ms 1000`), the agent in trial r2 did **not**
register a single `socket.timeout` during the 4.3 s pause. The
recv() call sat for the whole pause and returned the response
cleanly after resume.

Mechanism:

1. KVM pauses the source vCPU during `pause()`. The vCPU does not
   tick. No timer interrupts fire inside the guest.
2. The guest's CLOCK_MONOTONIC is derived from kvmclock, which is
   itself a function of the host TSC + a VM-specific offset.
   While the vCPU is suspended, the guest sees neither wall-clock
   nor monotonic advance.
3. Socket timeouts in Linux are scheduled via the guest's timer
   wheel. With the vCPU suspended, the timer doesn't fire.
4. When `resume()` reschedules the vCPU, kvmclock's offset is
   adjusted so the guest's wall-clock catches up to the host —
   so `time.time()` in the guest does observe the gap (the agent's
   `t_recv_ms` shows the pause). But CLOCK_MONOTONIC catches up
   *atomically*, by which time the response data has arrived in
   the socket buffer. The recv() returns data before the timer
   gets a chance to fire.
5. Result: the agent's `t_recv_ms` shows the pause, but its
   in-flight recv() does not raise `socket.timeout`.

This means **forkd v0.2 branching is way more agent-friendly
than expected for purely in-guest workloads**. Long-running
agents holding TCP sockets to other processes inside the guest
(e.g., a python agent talking to a sidecar) are effectively
pause-blind.

The *pain* lives elsewhere:

- **External-peer-visible latency.** The host-side echo server
  (and any real-world external service the agent is talking to)
  sees the 4 s gap. From their POV the agent is offline for 4 s.
- **Inbound buffering.** Packets to the guest during the pause
  pile up in the tap device's queue and may be dropped if the
  queue overflows. Our 100 ms cadence × 4 s = 40 unacked frames,
  well under any sane tx queue depth, so we didn't see drops.
- **Application-level keepalives.** Real LLM API clients have
  client-side timeouts (OpenAI's default is 600 s; Anthropic
  10 min). A 4 s pause is invisible to those, but tight gRPC
  deadlines (1–5 s) would be killed by it.

## Implications for v0.3 userfaultfd

This data sharpens the v0.3 problem statement:

| Audience | Pause-sensitivity today | Userfaultfd wins? |
|---|---|---|
| In-guest agent holding TCP to another in-guest process | **None** (pause-blind via mechanism above) | Marginal |
| In-guest agent on a long-poll HTTP call to an external API | Adds N s to the round-trip but doesn't fail | Yes, lowers user-visible latency |
| Agent serving a real-time stream (WebSocket clients listening to it) | **High** — clients see N s of dead air | **Big win** |
| Agent with a tight gRPC deadline to a peer | **Critical** — call fails | **Big win** |

If you can keep your latency budget to "agents tolerate 4 s of
silence", forkd v0.2 is enough. If you need sub-second
branching for real-time / tight-deadline workloads, v0.3's
userfaultfd path is the right bet.

## What's missing (v0.3 follow-ups)

1. **Memory-size sweep.** Disk write speed dominates the pause
   window. Map the curve across 256 MiB / 1 GiB / 4 GiB / 8 GiB.
   Expect linear-ish, but the constant + jitter matters for
   the paper figure.
2. **Disk type sweep.** The 4 s for 513 MiB implies ~125 MiB/s
   write speed. NVMe should be 4–10× faster; tmpfs would
   essentially eliminate disk from the budget. Worth disambiguating.
3. **Host-side analysis in `analyze.py`.** Currently the analyzer
   uses agent JSONL only. The echo server's JSONL has independent
   host-side timestamps that can confirm the in-guest skew
   automatically.
4. **Multiple in-flight requests.** Our agent pipelines = 1. With
   N concurrent requests, the in-flight loss math becomes
   non-trivial — a real measurement instead of always-zero.
5. **Realistic agent workload.** Replace the synthetic ping/pong
   with one of the `recipes/` workloads (LangGraph agent doing
   actual LLM calls) to validate the in-guest pause-blindness
   claim against richer applications.

(1) and (2) are the most paper-relevant. (5) is the social-media
demo. (3) and (4) are engineering tidying.

## Run command for reproducibility

```bash
# On the dev box, with daemon running:
export FORKD_URL=http://127.0.0.1:8889
export FORKD_TOKEN=$(cat /etc/forkd/token)

for i in 1 2 3 4 5; do
  bash bench/pause-window/run.sh \
    --snapshot-tag benchsrc \
    --duration-s 30 \
    --branch-at-s 10 \
    --out /tmp/bench-pause/r$i
done
```

`benchsrc` is the snapshot built from
`python:3.12-slim` + `python3`. See `scripts/build-rootfs.sh`.
