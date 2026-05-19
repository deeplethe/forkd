#!/usr/bin/env python3
"""Generate v0.3 ship charts from RESULTS-v0.3.md sweep CSVs.

Outputs (in this directory):
  pause-vs-mem-ssd.png       — Chart A: Full vs Diff at 5 memory sizes
  pause-vs-dirty-ssd.png     — Chart B: pause vs dirty-footprint curve

Run from inside the docs/assets/charts/ directory:
  python3 gen_charts.py

Reads:
  ../../../bench/pause-window/diff-real-sweep-ssd.csv
  ../../../bench/pause-window/agent-sweep-ssd.csv
"""
import csv
from collections import defaultdict
from pathlib import Path

import matplotlib.pyplot as plt
import numpy as np

HERE = Path(__file__).parent
BENCH = HERE / "../../../bench/pause-window"

# Consistent styling — readable on both light + dark backgrounds.
plt.rcParams.update(
    {
        "figure.dpi": 150,
        "font.size": 11,
        "axes.spines.top": False,
        "axes.spines.right": False,
        "axes.grid": True,
        "grid.alpha": 0.25,
    }
)
FULL_COLOR = "#888888"
DIFF_COLOR = "#0a84ff"


def load_csv(path: Path) -> list[dict[str, str]]:
    with open(path, newline="") as f:
        return list(csv.DictReader(f))


def chart_a():
    """Bar chart: Full vs Diff at 5 memory sizes, SSD backend."""
    rows = load_csv(BENCH / "diff-real-sweep-ssd.csv")
    # Group by (memory_mib, mode), average pause_ms
    bucket: dict[tuple[int, str], list[float]] = defaultdict(list)
    for r in rows:
        bucket[(int(r["memory_mib"]), r["mode"])].append(float(r["pause_ms"]))

    sizes = sorted({k[0] for k in bucket})
    full = [np.mean(bucket[(s, "full")]) / 1000 for s in sizes]  # → seconds
    diff = [np.mean(bucket[(s, "diff")]) / 1000 for s in sizes]
    speedup = [f / d for f, d in zip(full, diff)]

    fig, ax = plt.subplots(figsize=(8, 4.5))
    x = np.arange(len(sizes))
    w = 0.38

    bars_full = ax.bar(x - w / 2, full, w, label="v0.2 Full", color=FULL_COLOR)
    bars_diff = ax.bar(x + w / 2, diff, w, label="v0.3 Diff", color=DIFF_COLOR)

    ax.set_xticks(x)
    ax.set_xticklabels([f"{s} MiB" for s in sizes])
    ax.set_xlabel("Source memory size")
    ax.set_ylabel("Source pause (seconds)")
    ax.set_yscale("log")
    ax.set_title("forkd v0.3 BRANCH: source-pause window — Full vs Diff (SATA SSD, idle source)")
    ax.legend(loc="upper left", frameon=False)

    # Annotate Diff bars with the speedup labels.
    for i, (b_full, b_diff, sp) in enumerate(zip(bars_full, bars_diff, speedup)):
        # Speedup text just above each Diff bar.
        ax.text(
            x[i] + w / 2,
            diff[i] * 1.15,
            f"{sp:.0f}×",
            ha="center",
            va="bottom",
            fontsize=10,
            color=DIFF_COLOR,
            fontweight="bold",
        )
        # Full bar value above (small).
        ax.text(
            x[i] - w / 2,
            full[i] * 1.05,
            f"{full[i]:.1f}s",
            ha="center",
            va="bottom",
            fontsize=8,
            color=FULL_COLOR,
        )
        # Diff bar value inside or below.
        ax.text(
            x[i] + w / 2,
            diff[i] * 0.6,
            f"{diff[i]*1000:.0f}ms",
            ha="center",
            va="top",
            fontsize=8,
            color="white",
            fontweight="bold",
        )

    ax.set_ylim(0.1, 60)
    fig.tight_layout()
    out = HERE / "pause-vs-mem-ssd.png"
    fig.savefig(out, bbox_inches="tight")
    plt.close(fig)
    print(f"wrote {out}")


def chart_b():
    """Line chart: pause vs dirty footprint (mem-2048 SSD)."""
    rows = load_csv(BENCH / "agent-sweep-ssd.csv")
    bucket: dict[tuple[int, str], list[float]] = defaultdict(list)
    for r in rows:
        bucket[(int(r["dirty_mib"]), r["mode"])].append(float(r["pause_ms"]))

    dirty = sorted({k[0] for k in bucket})
    full = [np.mean(bucket[(d, "full")]) / 1000 for d in dirty]
    diff = [np.mean(bucket[(d, "diff")]) / 1000 for d in dirty]

    fig, ax = plt.subplots(figsize=(8, 4.5))
    ax.plot(dirty, full, "o-", color=FULL_COLOR, linewidth=2, markersize=7, label="v0.2 Full")
    ax.plot(dirty, diff, "o-", color=DIFF_COLOR, linewidth=2, markersize=7, label="v0.3 Diff")

    # Find approximate crossover (linear interpolation on the two lists).
    # Diff slope ≈ ~10 ms/MiB above ~100 MiB; intersects Full at ~1300 MiB.
    cross_mib = None
    for i in range(1, len(dirty)):
        if diff[i] >= full[i] * 0.95:
            cross_mib = dirty[i]
            break

    ax.set_xlabel("Dirty footprint (MiB)")
    ax.set_ylabel("Source pause (seconds)")
    ax.set_title("forkd v0.3 BRANCH: pause vs dirty footprint (mem-2048 SSD)")
    ax.legend(loc="upper left", frameon=False)

    # Speedup annotations on a few key points.
    annot_dirty = [0, 100, 500, 1000]
    for d in annot_dirty:
        if d in dirty:
            i = dirty.index(d)
            sp = full[i] / diff[i]
            ax.annotate(
                f"{sp:.1f}×",
                xy=(d, diff[i]),
                xytext=(d, diff[i] * 0.35),
                ha="center",
                fontsize=10,
                color=DIFF_COLOR,
                fontweight="bold",
            )

    if cross_mib:
        ax.axvline(cross_mib, linestyle="--", color="gray", alpha=0.4)
        ax.text(
            cross_mib + 20,
            0.6,
            f"crossover\n~{cross_mib} MiB dirty",
            fontsize=9,
            color="gray",
            va="bottom",
        )

    ax.set_xlim(-30, 1100)
    ax.set_ylim(0.3, 20)
    ax.set_yscale("log")
    fig.tight_layout()
    out = HERE / "pause-vs-dirty-ssd.png"
    fig.savefig(out, bbox_inches="tight")
    plt.close(fig)
    print(f"wrote {out}")


if __name__ == "__main__":
    chart_a()
    chart_b()
