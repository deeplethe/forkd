#!/usr/bin/env python3
"""
Pause-window analyzer — parses the agent + echo-server JSONL logs
from one trial and emits a summary report.

The core function `analyze()` is a pure function over already-parsed
event lists so it's testable in isolation (see `test_analyze.py`).
The CLI shell at the bottom just loads files and prints.

Report shape (JSON):

{
  "total_duration_ms": int,
  "sent": int, "recv": int, "timeouts": int, "errors": int,
  "connection_survived": bool,
  "pause": {
    "detected": bool,
    "app_start_ms": int|null,        // last recv before the gap
    "app_end_ms":   int|null,        // first recv after the gap
    "app_duration_ms": int|null,     // gap - baseline_interval
    "in_flight_lost": int             // pings sent during gap with no recv
  },
  "rtt_ms": {
    "before_p50": float, "before_p99": float,
    "after_p50":  float, "after_p99":  float,
    "all_p50":    float, "all_p99":    float
  },
  "daemon_pause_ms": int|null        // ground truth from BRANCH response
}
"""
import argparse
import json
import sys
from pathlib import Path
from statistics import median
from typing import Optional


# Two consecutive recvs ~ baseline_interval_ms apart in a healthy stream.
# A gap > BASELINE_MULTIPLIER * baseline is flagged as a pause.
BASELINE_MULTIPLIER = 3.0


def pct(values, p: float) -> float:
    """Plain-stdlib percentile, p in [0, 100]. Empty → 0.0."""
    if not values:
        return 0.0
    s = sorted(values)
    if p <= 0:
        return float(s[0])
    if p >= 100:
        return float(s[-1])
    # Linear interp, matches numpy.percentile default.
    idx = (p / 100.0) * (len(s) - 1)
    lo = int(idx)
    hi = min(lo + 1, len(s) - 1)
    frac = idx - lo
    return float(s[lo] * (1 - frac) + s[hi] * frac)


def analyze(
    agent_events: list,
    server_events: Optional[list] = None,
    daemon_pause_ms: Optional[int] = None,
    baseline_interval_ms: int = 100,
) -> dict:
    """Pure-function summary. server_events currently unused but
    accepted so future analyses (clock-skew estimation, lost-on-
    server-side counting) can plug in without changing the call
    site."""
    _ = server_events  # noqa: reserved for future use

    start = next((e for e in agent_events if e.get("event") == "start"), None)
    stop = next((e for e in agent_events if e.get("event") == "stop"), None)
    sends = [e for e in agent_events if e.get("event") == "send"]
    recvs = [e for e in agent_events if e.get("event") == "recv"]
    timeouts = [e for e in agent_events if e.get("event") == "timeout"]
    errors = [e for e in agent_events if e.get("event") == "error"]

    total_duration_ms = 0
    if sends and recvs:
        total_duration_ms = max(e["t_recv_ms"] for e in recvs) - min(
            e["t_send_ms"] for e in sends
        )

    # Pause detection: find the largest gap between consecutive recv
    # events. If it exceeds BASELINE_MULTIPLIER × baseline_interval,
    # call it a pause.
    pause_app_start = None
    pause_app_end = None
    pause_duration = None
    in_flight_lost = 0

    if len(recvs) >= 2:
        recvs_sorted = sorted(recvs, key=lambda e: e["t_recv_ms"])
        gaps = []
        for a, b in zip(recvs_sorted, recvs_sorted[1:]):
            gap = b["t_recv_ms"] - a["t_recv_ms"]
            gaps.append((gap, a, b))
        gap, a, b = max(gaps, key=lambda x: x[0])
        threshold = baseline_interval_ms * BASELINE_MULTIPLIER
        if gap > threshold:
            pause_app_start = a["t_recv_ms"]
            pause_app_end = b["t_recv_ms"]
            pause_duration = max(0, gap - baseline_interval_ms)
            # Count sends in (pause_start, pause_end) with no
            # matching recv. Build a recv-seq set for lookup.
            recv_seqs = {e["seq"] for e in recvs}
            for s in sends:
                if pause_app_start < s["t_send_ms"] < pause_app_end:
                    if s["seq"] not in recv_seqs:
                        in_flight_lost += 1

    # Connection survived if we got recvs after the pause, OR if
    # there was no pause at all and the run ended cleanly.
    connection_survived = False
    if pause_app_end is not None:
        post_recvs = [e for e in recvs if e["t_recv_ms"] > pause_app_end]
        connection_survived = len(post_recvs) > 0
    elif stop is not None and stop.get("errors", 0) == 0:
        connection_survived = True

    rtts = [e["rtt_ms"] for e in recvs if "rtt_ms" in e]
    before = [e["rtt_ms"] for e in recvs if pause_app_start is None or e["t_recv_ms"] <= pause_app_start]
    after = [e["rtt_ms"] for e in recvs if pause_app_end is not None and e["t_recv_ms"] >= pause_app_end]

    return {
        "total_duration_ms": int(total_duration_ms),
        "sent": int(stop["sent"]) if stop else len(sends),
        "recv": int(stop["recv"]) if stop else len(recvs),
        "timeouts": int(stop["timeouts"]) if stop else len(timeouts),
        "errors": int(stop["errors"]) if stop else len(errors),
        "connection_survived": connection_survived,
        "pause": {
            "detected": pause_app_start is not None,
            "app_start_ms": pause_app_start,
            "app_end_ms": pause_app_end,
            "app_duration_ms": pause_duration,
            "in_flight_lost": in_flight_lost,
        },
        "rtt_ms": {
            "before_p50": round(pct(before, 50), 2),
            "before_p99": round(pct(before, 99), 2),
            "after_p50": round(pct(after, 50), 2),
            "after_p99": round(pct(after, 99), 2),
            "all_p50": round(pct(rtts, 50), 2),
            "all_p99": round(pct(rtts, 99), 2),
        },
        "daemon_pause_ms": daemon_pause_ms,
        "_meta": {
            "baseline_interval_ms": baseline_interval_ms,
            "baseline_multiplier": BASELINE_MULTIPLIER,
        },
    }


def load_jsonl(path: Path) -> list:
    out = []
    with path.open("r", encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            out.append(json.loads(line))
    return out


def format_md(report: dict) -> str:
    p = report["pause"]
    r = report["rtt_ms"]
    survived = "✅" if report["connection_survived"] else "❌"
    detected = "yes" if p["detected"] else "no"
    daemon = f"{report['daemon_pause_ms']} ms" if report["daemon_pause_ms"] is not None else "n/a"
    app_dur = f"{p['app_duration_ms']} ms" if p["app_duration_ms"] is not None else "n/a"

    return (
        f"# Pause-window trial report\n\n"
        f"| Metric | Value |\n"
        f"|---|---|\n"
        f"| Sent | {report['sent']} |\n"
        f"| Received | {report['recv']} |\n"
        f"| Timeouts | {report['timeouts']} |\n"
        f"| Errors | {report['errors']} |\n"
        f"| Connection survived | {survived} |\n"
        f"| Pause detected | {detected} |\n"
        f"| Daemon-measured pause | {daemon} |\n"
        f"| App-observed pause | {app_dur} |\n"
        f"| In-flight requests lost | {p['in_flight_lost']} |\n"
        f"| RTT p50 (before pause) | {r['before_p50']} ms |\n"
        f"| RTT p99 (before pause) | {r['before_p99']} ms |\n"
        f"| RTT p50 (after pause) | {r['after_p50']} ms |\n"
        f"| RTT p99 (after pause) | {r['after_p99']} ms |\n"
    )


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__.strip().splitlines()[0])
    p.add_argument("--agent-log", required=True, type=Path)
    p.add_argument("--server-log", type=Path, default=None)
    p.add_argument(
        "--daemon-pause-ms",
        type=int,
        default=None,
        help="Daemon ground-truth pause window from the BRANCH response",
    )
    p.add_argument(
        "--baseline-interval-ms",
        type=int,
        default=100,
        help="Expected interval between recv events (default 100, must match agent --interval-ms)",
    )
    p.add_argument("--out-json", type=Path, default=None)
    p.add_argument("--out-md", type=Path, default=None)
    args = p.parse_args()

    agent_events = load_jsonl(args.agent_log)
    server_events = load_jsonl(args.server_log) if args.server_log else None
    report = analyze(
        agent_events,
        server_events,
        daemon_pause_ms=args.daemon_pause_ms,
        baseline_interval_ms=args.baseline_interval_ms,
    )

    if args.out_json:
        args.out_json.write_text(json.dumps(report, indent=2), encoding="utf-8")
    if args.out_md:
        args.out_md.write_text(format_md(report), encoding="utf-8")
    if not args.out_json and not args.out_md:
        print(json.dumps(report, indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main())
