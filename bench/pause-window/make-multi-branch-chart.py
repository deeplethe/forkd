"""
Generate v0.3.3 vs v0.3.4 multi-BRANCH pause-time comparison chart.
Source: bench/pause-window/RESULTS-v0.3.md (10-BRANCH sweep table).
"""

import matplotlib.pyplot as plt
import matplotlib.ticker as mticker

# Data from bench/pause-window/RESULTS-v0.3.md (10-BRANCH sweep).
branch_idx = list(range(1, 11))
before     = [350, 250, 1300, 1400, 1500, 2700, 1500, 1800, 2700, 1500]
after      = [585, 286,  344,  161,  369,  153,  189,  162,  324,  174]

# Square-ish aspect for mobile feeds (1080x1080).
fig, ax = plt.subplots(figsize=(10.8, 10.8), dpi=100)

# Plot lines.
ax.plot(branch_idx, before, marker="o", markersize=12, linewidth=3.5,
        color="#d63031", label="v0.3.3 (before fix)")
ax.plot(branch_idx, after,  marker="s", markersize=12, linewidth=3.5,
        color="#0984e3", label="v0.3.4 (after fix)")

# Fill area between for visual punch.
ax.fill_between(branch_idx, before, after, color="#d63031", alpha=0.08)

# Annotation arrow on BRANCH 6 (the headline 17.6x).
ax.annotate(
    "BRANCH 6:\n2700 ms → 153 ms\n17.6× faster",
    xy=(6, 153), xytext=(6.8, 1900),
    fontsize=22, fontweight="bold", color="#2d3436",
    ha="left", va="center",
    arrowprops=dict(arrowstyle="->", color="#2d3436", lw=2.5,
                    connectionstyle="arc3,rad=0.2"),
    bbox=dict(boxstyle="round,pad=0.6", facecolor="#fdcb6e", edgecolor="#2d3436", lw=1.5),
)

# Axes.
ax.set_xlabel("BRANCH index (consecutive on same source)", fontsize=20, labelpad=12)
ax.set_ylabel("pause time (ms)", fontsize=20, labelpad=12)
ax.set_xticks(branch_idx)
ax.tick_params(axis="both", labelsize=18)
ax.yaxis.set_major_formatter(mticker.FuncFormatter(lambda x, _: f"{int(x):,}"))
ax.set_ylim(0, 3200)
ax.set_xlim(0.5, 10.5)

# Grid for readability.
ax.grid(True, axis="y", linestyle="--", alpha=0.3)
ax.set_axisbelow(True)

# Title + subtitle.
fig.suptitle(
    "sandstorm v0.3.4: multi-BRANCH pause anomaly resolved",
    fontsize=28, fontweight="bold", y=0.965,
)
ax.set_title(
    "30-line `posix_fallocate` fix bypasses ext4 metadata compounding\n"
    "(Ubuntu 24.04, kernel 6.14, i7-12700, ext4 on SSD)",
    fontsize=18, color="#636e72", pad=18,
)

# Legend.
ax.legend(loc="upper right", fontsize=20, frameon=True, fancybox=True,
          framealpha=0.95, edgecolor="#2d3436")

# Footer.
fig.text(0.5, 0.02,
         "Issue #146  ·  PR #152  ·  github.com/deeplethe/sandstorm",
         ha="center", fontsize=14, color="#636e72")

plt.tight_layout(rect=[0, 0.03, 1, 0.94])

import pathlib
out_path = pathlib.Path(__file__).parent / "v0.3.4-before-after.png"
plt.savefig(out_path, dpi=100, bbox_inches="tight", facecolor="white")
print(f"Saved: {out_path}")
print(f"BRANCH 6 speedup: {before[5]/after[5]:.1f}x")
print(f"Median BRANCH 3-10 speedup: "
      f"{sorted(before[2:])[3] / sorted(after[2:])[3]:.1f}x")
