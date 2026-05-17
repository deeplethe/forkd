# Pause-window benchmark

How long does the BRANCH pause window actually hurt? The
[v0.3 userfaultfd bet](../../docs/ROADMAP.md) commits 4–6 weeks of
engineering to shrink it. This benchmark is the **problem
statement**: it quantifies what application-level damage a 0.5–8 s
pause does to a running agent. Without this number the userfaultfd
work has no justification; with this number it has a paper §2.

## What it measures

A pretend-agent inside the source sandbox holds an open TCP
connection to a host-side echo server and sends a timestamped
16-byte frame every `--interval-ms` (default 100 ms). Mid-run, the
orchestrator calls `POST /v1/sandboxes/:id/branch`, which pauses
the source for the duration of the snapshot. The agent keeps
logging every send / recv / timeout to JSONL.

We then compute, per trial:

| Metric | What it tells us |
|---|---|
| Daemon-reported `pause_ms` | Ground truth: `pause() → resume()` envelope on the source VM |
| App-observed pause | Gap between last pre-pause recv and first post-pause recv |
| In-flight loss | Pings sent during the pause that never got a response |
| Connection survived | Did the TCP socket recover, or did the peer / agent give up? |
| Post-resume RTT p99 | OS retransmit timers / TCP slow-start tail after pause |

The **gap** between daemon-reported and app-observed pause is the
interesting number — it's the OS retransmit overhead the userfaultfd
work won't eliminate. If they're nearly equal, the pause-time line
in our paper *is* the user-visible cost. If they diverge (e.g.,
app observed 6 s while daemon paused 2 s), we have to explain the
retransmit-timer story.

## Running one trial

Requires:
- A forkd-controller running, with `FORKD_URL` + `FORKD_TOKEN` set
- A snapshot tag with a python3-capable rootfs already built
  (the `recipes/postgres-fixture/` rootfs works, or any pyagent
  rootfs from `bench/`)
- `jq` and `curl` on the host

```bash
export FORKD_URL=http://127.0.0.1:8889
export FORKD_TOKEN=$(cat /etc/forkd/token)

# One 60s trial with BRANCH at t=30s
bench/pause-window/run.sh --snapshot-tag pyagent --duration-s 60 --branch-at-s 30
```

Output lands in `bench/pause-window/results/<tag>-<unix-ts>/`:

```
spawn.json     # POST /v1/sandboxes response
exec.json      # exec output (stdout = agent.jsonl, stderr separate)
branch.json    # POST /v1/sandboxes/:id/branch response, with pause_ms
agent.jsonl    # in-sandbox per-frame send/recv events
server.jsonl   # host-side echo events
report.json    # parsed metrics
report.md      # human-readable summary table
```

`report.md` is what you'd paste into a paper §2 figure caption.

## Sweep across memory sizes (planned)

The pause window scales roughly linearly with the source VM's
memory image. Run the same trial against rootfses of different
memory sizes to map the curve:

```bash
for mem in 256M 1G 2G 8G; do
  bench/pause-window/run.sh --snapshot-tag "pyagent-$mem" --out "results/$mem/"
done
```

A `sweep.py` driver that aggregates the per-trial reports into one
table is a [v0.3 follow-up](../../docs/ROADMAP.md) — for now the
single-trial harness is what we need to validate the methodology.

## Two configurations to compare

Run each trial twice with different `--read-timeout-ms`:

| Mode | `--read-timeout-ms` | What it models |
|---|---|---|
| Patient agent | 30000 (default) | LLM workflow with long-poll on completions API |
| Tight agent | 1000 | Agent with sub-second SLAs (real-time streaming, rate-limited) |

The "tight" mode is where the pause window matters most. A 2 s
pause is tolerable for a patient agent but kills a tight one.

## How the analyzer detects the pause

`analyze.py` doesn't read the daemon log — it works on the agent's
JSONL alone, to mirror what an external operator could measure.

1. Sort all `recv` events by `t_recv_ms`.
2. For each adjacent pair, compute the gap in ms.
3. The largest gap, if it exceeds `BASELINE_MULTIPLIER × interval_ms`
   (default 3×), is the pause window.
4. App-observed duration = `gap − interval_ms` (subtracting the
   one expected gap that always exists between sends).
5. In-flight loss = sends made between the gap's endpoints that
   never produced a matching recv (the agent reports these as
   `timeout` events; we cross-check by `seq` lookup).

Daemon-reported `pause_ms` is passed in separately so the report
can compare both.

See [`test_analyze.py`](./test_analyze.py) for 14 unit tests
covering clean runs, paused runs, percentile math, and
baseline-sensitivity.

## Why not just use `iperf` or `tc netem`?

`iperf` measures throughput; we want round-trip latency tail.
`tc netem` simulates packet loss / delay but doesn't model the
specific "OS scheduler suspended, sockets in kernel queue stay
warm, no app threads to drain them" pattern that BRANCH produces.
The custom agent + echo server is the smallest faithful model.

## Why not run inside the postgres recipe directly?

We will, in a v0.3 follow-up. For the v0.2-era problem statement
the synthetic ping/pong is cleaner: one cause of stall, one stream
of evidence, no postgres-specific quirks (vacuum timing, autovacuum,
checkpoint flush latency) confusing the picture.
