#!/usr/bin/env python3
"""Render the forkd-vs-others benchmark charts.

Produces:
  bench/chart-spawn-100.png      bar, N=100 spawn time, log scale
  bench/chart-memory-per.png     bar, host memory delta per sandbox

All numbers come from runs on a single host (Ubuntu 24.04 / Linux 6.14
/ 20 vCPU / 30 GiB / KVM). Override via env BENCH_RESULTS=path.json
produced by bench/compare-all.py.
"""

import argparse
import json
import os
import sys
from pathlib import Path

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt


# All numbers below are end-to-end wall-clock measurements of the same
# workload — spawn N=100 sandboxes that each try `import numpy;
# numpy.zeros(5).tolist()` — on the same Ubuntu 24.04 / Linux 6.14 /
# 20 vCPU / 30 GiB / KVM host. Override via BENCH_RESULTS env at
# render time.
#
# Sources / measurement notes:
#   forkd        — measured. fork-from-warm via snapshot CoW; the parent
#                  imported numpy once before being snapshotted.
#   BoxLite      — measured (boxlite 0.9.3 Python SDK, asyncio.gather of
#                  N SimpleBox(python:3.12-slim).exec("import numpy")).
#                  Each Box is a fresh KVM microVM running an OCI rootfs.
#   CubeSandbox  — measured (cube-sandbox-one-click v0.2.0, cube-api on
#                  127.0.0.1:6000, asyncio.gather of POST /sandboxes
#                  with template forkd-bench-pynp). Fast-path number:
#                  N=100 wall-clock = 1056 ± 14 ms over five runs,
#                  100 % success every run, measured with an improved
#                  bench script that pre-warms Python's default
#                  ThreadPoolExecutor. v0.2.0 carries a ~50 ms latency
#                  regression that PR #234 fixes in v0.2.1; the value
#                  here is the v0.2.0 baseline. See CUBESANDBOX.md for
#                  the slow-path row and the upstream cmdTimeout race.
#   OpenSandbox  — measured (opensandbox-server 0.1.8, Docker runtime,
#                  127.0.0.1:18080, asyncio.gather of N Sandbox.create
#                  with image=python:3.12-slim). See OPENSANDBOX.md.
#   Firecracker  — measured. Cold-boot one VM per sandbox.
#   gVisor       — measured. runsc OCI runtime at N=100.
#   Docker       — measured. runc OCI runtime at N=100.
DATA = {
    "forkd":       {"label": "forkd",            "spawn_ms_100":    101, "mem_per_mib": 0.12, "color": "#4c956c", "highlight": True},
    "firecracker": {"label": "Firecracker cold", "spawn_ms_100":    759, "mem_per_mib": 84.3, "color": "#8d99ae"},
    "cubesandbox": {"label": "CubeSandbox*",     "spawn_ms_100":   1056, "mem_per_mib":  5.0, "color": "#8d99ae"},
    "boxlite":     {"label": "BoxLite",          "spawn_ms_100": 113209, "mem_per_mib": None, "color": "#8d99ae"},
    "opensandbox": {"label": "OpenSandbox",      "spawn_ms_100": 121958, "mem_per_mib": None, "color": "#8d99ae"},
    "gvisor":      {"label": "gVisor (runsc)",   "spawn_ms_100": 288557, "mem_per_mib": None, "color": "#8d99ae"},
    "docker":      {"label": "Docker (runc)",    "spawn_ms_100": 335278, "mem_per_mib":  4.3, "color": "#8d99ae"},
}


def load_results():
    """Optionally override DATA from a JSON file produced by compare-all.py."""
    path = os.environ.get("BENCH_RESULTS")
    if not path or not Path(path).exists():
        return DATA
    with open(path) as f:
        results = json.load(f)
    data = dict(DATA)
    for r in results:
        if r.get("error"):
            continue
        backend = r.get("backend")
        if backend in data:
            data[backend] = {**data[backend], "spawn_ms_100": r["total_ms"]}
    return data


# Neutral, print-friendly palette. White background, dark text, no
# arrows or "← baseline" rhetoric. Highlight the forkd bar with a
# saturated green; everything else stays muted.
BG_FACE = "#ffffff"
TEXT    = "#1f2933"
GRID    = "#cbd2d9"
MUTED   = "#52606d"


def style_axes(ax, fig):
    fig.patch.set_facecolor(BG_FACE)
    ax.set_facecolor(BG_FACE)
    ax.tick_params(colors=TEXT, labelsize=11)
    for side, spine in ax.spines.items():
        if side in ("top", "right"):
            spine.set_visible(False)
        else:
            spine.set_color(GRID)
    ax.grid(axis="x", which="both", color=GRID, alpha=0.4, linewidth=0.6)


def fmt_ms(ms):
    if ms is None:
        return "—"
    if ms >= 10_000:
        return f"{ms / 1000:.1f} s"
    return f"{ms} ms"


def chart_spawn(data, out):
    items = [(k, v) for k, v in data.items() if v["spawn_ms_100"] is not None]
    items.sort(key=lambda kv: kv[1]["spawn_ms_100"])

    labels = [v["label"] for _, v in items]
    times  = [v["spawn_ms_100"] for _, v in items]
    colors = [v["color"] for _, v in items]

    fig, ax = plt.subplots(figsize=(10, 0.7 * len(labels) + 2.5))
    style_axes(ax, fig)

    bars = ax.barh(labels, times, color=colors, height=0.55, edgecolor="none")
    ax.set_xscale("log")
    ax.set_xlabel("Wall-clock to spawn 100 sandboxes ready to run numpy (ms, log scale)",
                  color=TEXT, fontsize=11)

    for bar, ms in zip(bars, times):
        ax.text(ms * 1.06, bar.get_y() + bar.get_height() / 2,
                fmt_ms(ms),
                color=TEXT, fontsize=11, va="center")

    ax.set_title("Spawn 100 sandboxes that import numpy",
                 color=TEXT, fontsize=13, weight="bold", pad=18, loc="left")
    fig.text(0.012, 0.012,
             "Host: Ubuntu 24.04 · Linux 6.14 · 20 vCPU · 30 GiB · KVM    "
             "*CubeSandbox: fast-path measurement (pool entry reused per sandbox); "
             "1056 ± 14 ms over five N=100 runs, 100 % success. See CUBESANDBOX.md.",
             color=MUTED, fontsize=9)
    plt.tight_layout(rect=(0, 0.03, 1, 1))
    plt.savefig(out, dpi=150, facecolor=fig.get_facecolor())
    print(f"wrote {out}", flush=True)


def chart_memory(data, out):
    items = [(k, v) for k, v in data.items() if v.get("mem_per_mib") is not None]
    items.sort(key=lambda kv: kv[1]["mem_per_mib"])

    labels = [v["label"] for _, v in items]
    mibs   = [v["mem_per_mib"] for _, v in items]
    colors = [v["color"] for _, v in items]

    fig, ax = plt.subplots(figsize=(10, 0.7 * len(labels) + 2.5))
    style_axes(ax, fig)

    bars = ax.barh(labels, mibs, color=colors, height=0.55, edgecolor="none")
    ax.set_xscale("log")
    ax.set_xlabel("Host memory delta per sandbox at N=100 (MiB, log scale)",
                  color=TEXT, fontsize=11)

    for bar, mib in zip(bars, mibs):
        text = f"{mib:.2f} MiB" if mib < 1 else f"{mib:.0f} MiB"
        ax.text(mib * 1.12, bar.get_y() + bar.get_height() / 2,
                text, color=TEXT, fontsize=11, va="center")

    ax.set_title("Host memory cost per sandbox (N=100)",
                 color=TEXT, fontsize=13, weight="bold", pad=18, loc="left")
    fig.text(0.012, 0.012,
             "Each forkd child mmaps the parent memory image MAP_PRIVATE; only "
             "diverged pages are private to the child.",
             color=MUTED, fontsize=9)
    plt.tight_layout(rect=(0, 0.03, 1, 1))
    plt.savefig(out, dpi=150, facecolor=fig.get_facecolor())
    print(f"wrote {out}", flush=True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out-dir", default=str(Path(__file__).parent))
    args = ap.parse_args()
    out_dir = Path(args.out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)

    data = load_results()
    chart_spawn(data,  out_dir / "chart-spawn-100.png")
    chart_memory(data, out_dir / "chart-memory-per.png")


if __name__ == "__main__":
    main()
