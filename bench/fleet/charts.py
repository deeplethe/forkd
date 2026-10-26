#!/usr/bin/env python3
"""Draw the README's fleet charts from a results directory.

    python3 bench/fleet/charts.py bench/fleet/results/2026-10-26 docs/assets/charts

Writes each chart twice, `-light.svg` and `-dark.svg`, for a `<picture>`
to choose between. Stdlib only, like loadtest.py: the SVG is written by
hand so regenerating the charts needs nothing installed.
"""

import csv
import sys
from pathlib import Path

FONT = 'system-ui, -apple-system, "Segoe UI", Helvetica, Arial, sans-serif'

# Categorical slots 1-3, stepped separately for each surface. Checked with
# the dataviz validator in both modes: adjacent CVD dE >= 9.2. Aqua is under
# 3:1 on the light surface, which is why every series is labelled directly.
THEMES = {
    "light": {
        "surface": "#fcfcfb",
        "ink": "#0b0b0b",
        "ink2": "#52514e",
        "muted": "#898781",
        "grid": "#e1e0d9",
        "axis": "#c3c2b7",
        "series": ["#2a78d6", "#eb6834", "#1baf7a"],
    },
    "dark": {
        "surface": "#1a1a19",
        "ink": "#f0efec",
        "ink2": "#c3c2b7",
        "muted": "#898781",
        "grid": "#2c2c2a",
        "axis": "#383835",
        "series": ["#3987e5", "#d95926", "#199e70"],
    },
}

W, H = 800, 380
LEFT, RIGHT, TOP, BOTTOM = 64, 24, 96, 48


def svg(theme, title, subtitle, body):
    t = THEMES[theme]
    return f"""<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {H}" width="{W}" height="{H}" font-family='{FONT}' role="img" aria-label="{title}">
<rect width="{W}" height="{H}" rx="8" fill="{t['surface']}"/>
<text x="{LEFT}" y="34" font-size="17" font-weight="600" fill="{t['ink']}">{title}</text>
<text x="{LEFT}" y="56" font-size="13" fill="{t['ink2']}">{subtitle}</text>
{body}
</svg>
"""


def y_axis(t, ymax, step, scale, fmt=lambda v: f"{v:,}"):
    out = []
    v = 0
    while v <= ymax:
        y = scale(v)
        colour = t["axis"] if v == 0 else t["grid"]
        out.append(f'<line x1="{LEFT}" x2="{W - RIGHT}" y1="{y:.1f}" y2="{y:.1f}" stroke="{colour}" stroke-width="1"/>')
        out.append(f'<text x="{LEFT - 8}" y="{y + 4:.1f}" font-size="11" fill="{t["muted"]}" text-anchor="end">{fmt(v)}</text>')
        v += step
    return out


def legend(t, items, x=LEFT, y=76):
    out = []
    for colour, label in items:
        out.append(f'<rect x="{x}" y="{y - 9}" width="10" height="10" rx="2" fill="{colour}"/>')
        out.append(f'<text x="{x + 15}" y="{y}" font-size="12" fill="{t["ink2"]}">{label}</text>')
        x += 22 + 7 * len(label)
    return out


def bar(x, y0, y1, w, colour, r=4):
    """A bar from the baseline y0 up to y1, rounded at the data end only."""
    h = y0 - y1
    r = min(r, h, w / 2)
    return (
        f'<path d="M{x:.1f},{y0:.1f} V{y1 + r:.1f} Q{x:.1f},{y1:.1f} {x + r:.1f},{y1:.1f} '
        f'H{x + w - r:.1f} Q{x + w:.1f},{y1:.1f} {x + w:.1f},{y1 + r:.1f} V{y0:.1f} Z" fill="{colour}"/>'
    )


def per_node(rows, theme):
    t = THEMES[theme]
    resident, tiered = t["series"][0], t["series"][1]
    ymax = 1400
    base = H - BOTTOM
    scale = lambda v: base - (base - TOP) * v / ymax
    body = y_axis(t, ymax, 200, scale)
    body += legend(t, [(resident, "every sandbox resident"), (tiered, "idle ones tiered down")])

    slot = (W - LEFT - RIGHT) / len(rows)
    bw = 30
    for i, r in enumerate(rows):
        cx = LEFT + slot * (i + 0.5)
        a, b = int(r["all_resident"]), int(r["tiered"])
        # 2 px of surface between the pair.
        body.append(bar(cx - bw - 1, base, scale(a), bw, resident))
        body.append(bar(cx + 1, base, scale(b), bw, tiered))
        body.append(f'<text x="{cx - bw / 2 - 1:.1f}" y="{scale(a) - 6:.1f}" font-size="11" fill="{t["ink2"]}" text-anchor="middle">{a}</text>')
        body.append(f'<text x="{cx + bw / 2 + 1:.1f}" y="{scale(b) - 6:.1f}" font-size="11" font-weight="600" fill="{t["ink"]}" text-anchor="middle">{b:,}</text>')
        body.append(f'<text x="{cx:.1f}" y="{base + 20}" font-size="12" fill="{t["muted"]}" text-anchor="middle">{r["node"]}</text>')

    total_b = sum(int(r["tiered"]) for r in rows)
    return svg(
        theme,
        "Sandboxes per machine",
        f"Six 2 vCPU / 1.9 GB nodes, gVisor. About 72 each with every sandbox resident; {total_b:,} in all with tiering.",
        "\n".join(body),
    )


def tiers(rows, theme):
    t = THEMES[theme]
    hot, frozen, snap = t["series"]
    ymax = 8000
    xmax = 570
    base = H - BOTTOM
    scale = lambda v: base - (base - TOP) * v / ymax
    xs = lambda s: LEFT + (W - LEFT - RIGHT) * s / xmax
    body = y_axis(t, ymax, 2000, scale)
    for m in range(0, 10, 2):
        x = xs(m * 60)
        body.append(f'<text x="{x:.1f}" y="{base + 20}" font-size="11" fill="{t["muted"]}" text-anchor="middle">{m} min</text>')

    pts = [(int(r["seconds"]), int(r["hot"]), int(r["frozen"]), int(r["snapshot"])) for r in rows]
    # Bottom to top: hot, frozen, snapshot. Each band is the area between
    # its running total and the one below; a surface-coloured edge on each
    # keeps the thin hot and frozen bands distinct from their neighbours.
    layers = [(hot, lambda p: p[1]), (frozen, lambda p: p[1] + p[2]), (snap, lambda p: p[1] + p[2] + p[3])]
    below = lambda p: 0
    for colour, top in layers:
        upper = " L".join(f"{xs(p[0]):.1f},{scale(top(p)):.1f}" for p in pts)
        lower = " L".join(f"{xs(p[0]):.1f},{scale(below(p)):.1f}" for p in reversed(pts))
        body.append(f'<path d="M{upper} L{lower} Z" fill="{colour}" stroke="{t["surface"]}" stroke-width="1" stroke-linejoin="round"/>')
        below = top

    # What the same six machines held with every sandbox resident.
    wall = 431
    y = scale(wall)
    body.append(f'<line x1="{LEFT}" x2="{W - RIGHT}" y1="{y:.1f}" y2="{y:.1f}" stroke="{t["ink"]}" stroke-width="1.5" stroke-dasharray="5 4"/>')
    body.append(f'<text x="{W - RIGHT}" y="{y - 6:.1f}" font-size="11" fill="{t["ink"]}" text-anchor="end">431: the same six machines, every sandbox resident</text>')

    peak = pts[-1]
    total = peak[1] + peak[2] + peak[3]
    body.append(f'<text x="{xs(peak[0]) - 4:.1f}" y="{scale(total) - 8:.1f}" font-size="13" font-weight="600" fill="{t["ink"]}" text-anchor="end">{total:,} running</text>')
    body += legend(t, [(hot, "hot"), (frozen, "frozen"), (snap, "snapshot")])
    return svg(
        theme,
        "Where the sandboxes are",
        "Creating at 15/s with nothing kept awake. Starts from the 3,399 a previous run left.",
        "\n".join(body),
    )


def main():
    src, dst = Path(sys.argv[1]), Path(sys.argv[2])
    dst.mkdir(parents=True, exist_ok=True)
    nodes = list(csv.DictReader(open(src / "per_node.csv")))
    timeline = list(csv.DictReader(open(src / "tiers.csv")))
    for theme in THEMES:
        (dst / f"fleet-per-node-{theme}.svg").write_text(per_node(nodes, theme))
        (dst / f"fleet-tiers-{theme}.svg").write_text(tiers(timeline, theme))
    print(f"wrote 4 charts to {dst}")


if __name__ == "__main__":
    main()
