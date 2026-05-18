# Pause-window: first-cut results (forkd v0.2)

Methodology validation on a single host, two storage backends.
Closes the v0.2-era pause-window question and seeds the v0.3
userfaultfd paper §2.

## TL;DR

For a 513 MiB source VM running a TCP ping/pong agent (one
outstanding request, 100 ms cadence), forkd's BRANCH pause window
is **dominated by snapshot-write throughput**, not by anything in
the VMM control path.

| Storage backend | Pause window (513 MiB source) | Trials |
|---|---:|:---:|
| **tmpfs (`/dev/shm`, ~4 GB/s)** | **163 ms ± 7 ms** | 157, 158, 165, 173 |
| SATA SSD on dev host (~150 MB/s fsync) | 4262 ms ± 414 ms | 4053, 4328, 3761, 4286, 4884 |

Same forkd code, same source memory, only `--snapshot-root`
changes. The **26x gap is entirely the storage layer**.

Two consistent observations across both backends:

1. **External observers see the pause as-is.** The host-side echo
   server sees a gap equal to the daemon's measured `pause_ms`.
2. **In-guest agents see almost nothing.** Connection survival
   5/5 (SSD trials), in-flight loss 0/5, post-resume RTT p99
   returns to baseline (1-2 ms) within one round-trip. The
   pause-blindness mechanism is described below.

## Setup

- Host: yangdongxu-desktop, Ubuntu 24.04, Linux 6.14, 20 vCPU, 30 GiB RAM, KVM enabled
- forkd-controller built from commit `fc0a0d2`
  (`security: validate snapshot_tag in create_sandbox + harden defaults` and ancestors)
- Source rootfs: `python:3.12-slim` + `python3` (built via
  `scripts/build-rootfs.sh`)
- Source memory: 513 MiB (firecracker default for this rootfs)
- Snapshot storage: two configurations measured separately:
  - SSD: `~/.local/share/forkd/snapshots/` on the host's
    `/dev/sda2` ext4 (SATA SSD, 148 MB/s fsync)
  - tmpfs: `/dev/shm/forkd-snap/` (RAM-backed, ~4 GB/s)

Each trial: spawn one source sandbox, run `agent.py` for 30 s
sending a 16-byte frame every 100 ms to a host-side
`echo_server.py`, trigger `POST /v1/sandboxes/:id/branch` at
t = 10 s, collect logs.

## SSD backend (slow-disk baseline, 5 trials)

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

## tmpfs backend (storage-bottleneck removed, 4 trials)

| Trial | Daemon `pause_ms` |
|---|---:|
| t1 | 158 ms |
| t2 | 157 ms |
| t3 | 165 ms |
| t4 | 173 ms |

**Stats across all 4:**
- Daemon pause: **mean 163 ms, std 7 ms, range 157-173 ms**
- **26x faster than the SSD configuration on identical source memory**

Same forkd code, same `langgraph` snapshot, same source memory
(513 MiB). Only `--snapshot-root` changes (from
`~/.local/share/forkd/snapshots/` on `/dev/sda2` to
`/dev/shm/forkd-snap/` on tmpfs).

## Why storage dominates

The in-VM work (pause vCPUs, harvest device state, resume) is
sub-millisecond. What takes time is writing 513 MiB of guest RAM
to `memory.bin` and waiting for the write to settle.

Direct measurement on the test host:

```
$ dd if=/dev/zero of=test.bin bs=1M count=512 conv=fsync
536870912 bytes (537 MB) copied, 3.638 s, 148 MB/s
```

SATA SSD does 148 MB/s with fsync. forkd's measured 128 MB/s
effective throughput on the SSD trials is consistent with this.
tmpfs has no fsync to wait on (RAM-backed), so the bound becomes
memcpy bandwidth, which the kernel hits at ~4 GB/s.

### What this means for production deployments

Three usable points on the curve, achievable today without
v0.3 work:

| Backend | Typical pause for 513 MiB | When to use |
|---|---:|---|
| SATA SSD (`fsync` ~150 MB/s) | ~4000 ms | Default, durable, cheapest hardware |
| NVMe (`fsync` 1-3 GB/s) | ~300-700 ms | Production hosts, persistent branches |
| tmpfs (`/dev/shm`) | ~160 ms | Ephemeral branches: speculative exploration, fan-out where the branch dies in seconds |

The tmpfs path is the right choice when branches are short-lived
and not meant to survive a host restart. For a "fork N agents,
let them explore, keep the best one's output, discard the rest"
workflow, the snapshot itself never needs durability. Put it on
tmpfs.

For production deployments where snapshots are catalog assets
(parents of many cold-start spawns), NVMe is the practical floor.

### Where each number sits in the published range

The forkd ROADMAP entry for v0.3 userfaultfd lists "0.5-8 s
depending on memory size" as the expected band for the current
algorithm. Our SSD number (4.26 s) is mid-band. Our tmpfs number
(163 ms) is the optimistic end of what's achievable without
changing the snapshot algorithm.

v0.3 userfaultfd aims for ~30 ms regardless of memory size. The
storage backend ladder above will still apply for the snapshot
write that happens after fork (children get a memory.bin
created by the userfault thread). Storage choice will continue
to matter; userfaultfd shrinks the *blocking* part of the pause.

### What we did NOT vary in this measurement

- Memory size. Still 513 MiB. Bigger sources should scale linearly
  with disk throughput until you saturate the controller.
- Source rootfs. Same `langgraph` snapshot, no recipe-specific
  variance.
- Number of dirty pages. The first BRANCH after a fresh boot
  writes near-full memory. Firecracker supports diff snapshots
  (write only changed pages since the previous snapshot); we
  have not measured that path here.
- Number of concurrent BRANCHes. Single-flight only.

These are v0.3 measurement gaps; the storage-backend axis was the
most impactful single variable, hence this section.

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
   adjusted so the guest's wall-clock catches up to the host.
   `time.time()` in the guest does observe the gap (the agent's
   `t_recv_ms` shows the pause). But CLOCK_MONOTONIC catches up
   atomically, by which time the response data has arrived in
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
| Agent serving a real-time stream (WebSocket clients listening to it) | **High**, clients see N s of dead air | **Big win** |
| Agent with a tight gRPC deadline to a peer | **Critical**, call fails | **Big win** |

If you can keep your latency budget to "agents tolerate 4 s of
silence", forkd v0.2 is enough. If you need sub-second
branching for real-time / tight-deadline workloads, v0.3's
userfaultfd path is the right bet.

## What's missing (v0.3 follow-ups)

1. **Memory-size sweep.** Disk write speed dominates the pause
   window. Map the curve across 256 MiB / 1 GiB / 4 GiB / 8 GiB.
   Expect linear-ish, but the constant + jitter matters for
   the paper figure.
2. **Disk type sweep (partially done).** SSD vs tmpfs measured in
   this doc shows the 26x gap. NVMe is the missing intermediate
   point. A production host with NVMe should land between the
   SSD and tmpfs numbers (typical 300-700 ms estimate based on
   1-3 GB/s fsync throughput).
3. **Host-side analysis in `analyze.py`.** Currently the analyzer
   uses agent JSONL only. The echo server's JSONL has independent
   host-side timestamps that can confirm the in-guest skew
   automatically.
4. **Multiple in-flight requests.** Our agent pipelines = 1. With
   N concurrent requests, the in-flight loss math becomes
   non-trivial: a real measurement instead of always-zero.
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
