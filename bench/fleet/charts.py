#!/usr/bin/env python3
"""Draw the README's fleet chart from a results directory.

    python3 bench/fleet/charts.py bench/fleet/results/2026-10-26 docs/assets/charts

Writes it twice, `-light.svg` and `-dark.svg`, for a `<picture>` to choose
between. Stdlib only, like loadtest.py: the SVG is written by hand so
redrawing it needs nothing installed.
"""

import csv
import sys
from pathlib import Path

FONT = 'system-ui, -apple-system, "Segoe UI", Helvetica, Arial, sans-serif'

THEMES = {
    "light": {
        "surface": "#fcfcfb",
        "ink": "#0b0b0b",
        "ink2": "#52514e",
        "muted": "#898781",
        "grid": "#ecebe6",
        "axis": "#c3c2b7",
        "line": "#2a78d6",
    },
    "dark": {
        "surface": "#1a1a19",
        "ink": "#f0efec",
        "ink2": "#c3c2b7",
        "muted": "#898781",
        "grid": "#262624",
        "axis": "#383835",
        "line": "#3987e5",
    },
}

W, H = 800, 400
LEFT, RIGHT, TOP, BOTTOM = 56, 32, 112, 44

# What the same six machines held with every sandbox kept in memory
# (t2_wall in the results).
ALL_RESIDENT = 431


def monotone(points):
    """An SVG path through `points` that never overshoots them.

    Fritsch-Carlson monotone cubic: smooth, but a count that holds flat is
    drawn flat, and the curve cannot dip below a value the data never had.
    """
    n = len(points)
    xs = [p[0] for p in points]
    ys = [p[1] for p in points]
    d = [(ys[i + 1] - ys[i]) / (xs[i + 1] - xs[i]) for i in range(n - 1)]
    m = [d[0]] + [0.0 if d[i - 1] * d[i] <= 0 else (d[i - 1] + d[i]) / 2 for i in range(1, n - 1)] + [d[-1]]
    for i in range(n - 1):
        if d[i] == 0:
            m[i] = m[i + 1] = 0.0
            continue
        a, b = m[i] / d[i], m[i + 1] / d[i]
        s = a * a + b * b
        if s > 9:
            t = 3 / s ** 0.5
            m[i], m[i + 1] = t * a * d[i], t * b * d[i]
    path = [f"M{xs[0]:.1f},{ys[0]:.1f}"]
    for i in range(n - 1):
        h = (xs[i + 1] - xs[i]) / 3
        path.append(
            f"C{xs[i] + h:.1f},{ys[i] + m[i] * h:.1f} "
            f"{xs[i + 1] - h:.1f},{ys[i + 1] - m[i + 1] * h:.1f} "
            f"{xs[i + 1]:.1f},{ys[i + 1]:.1f}"
        )
    return " ".join(path)


def running(rows, theme):
    t = THEMES[theme]
    ymax, xmax = 8000, 18 * 60
    base = H - BOTTOM
    sy = lambda v: base - (base - TOP) * v / ymax
    sx = lambda s: LEFT + (W - LEFT - RIGHT) * s / xmax
    pts = [(sx(int(r["seconds"])), sy(int(r["running"]))) for r in rows if int(r["seconds"]) <= xmax]
    peak = max(int(r["running"]) for r in rows)

    out = [
        f'<rect width="{W}" height="{H}" rx="12" fill="{t["surface"]}"/>',
        f'<defs><linearGradient id="fill" x1="0" y1="0" x2="0" y2="1">'
        f'<stop offset="0" stop-color="{t["line"]}" stop-opacity="0.28"/>'
        f'<stop offset="1" stop-color="{t["line"]}" stop-opacity="0"/></linearGradient></defs>',
        # The headline is the chart's own end value, so it cannot drift from it.
        f'<text x="{LEFT}" y="62" font-size="40" font-weight="700" fill="{t["ink"]}" letter-spacing="-0.5">{peak:,}</text>',
        f'<text x="{LEFT}" y="88" font-size="14" fill="{t["ink2"]}">sandboxes running on six 2 GB machines that hold {ALL_RESIDENT} kept in memory</text>',
    ]
    for v in range(0, ymax + 1, 2000):
        y = sy(v)
        out.append(f'<line x1="{LEFT}" x2="{W - RIGHT}" y1="{y:.1f}" y2="{y:.1f}" stroke="{t["axis"] if v == 0 else t["grid"]}"/>')
        label = "0" if v == 0 else f"{v // 1000}k"
        out.append(f'<text x="{LEFT - 10}" y="{y + 4:.1f}" font-size="12" fill="{t["muted"]}" text-anchor="end">{label}</text>')
    for m in range(0, 19, 3):
        out.append(f'<text x="{sx(m * 60):.1f}" y="{base + 24}" font-size="12" fill="{t["muted"]}" text-anchor="middle">{m} min</text>')

    curve = monotone(pts)
    out.append(f'<path d="{curve} L{pts[-1][0]:.1f},{base} L{pts[0][0]:.1f},{base} Z" fill="url(#fill)"/>')

    y = sy(ALL_RESIDENT)
    out.append(f'<line x1="{LEFT}" x2="{W - RIGHT}" y1="{y:.1f}" y2="{y:.1f}" stroke="{t["ink2"]}" stroke-width="1.25" stroke-dasharray="4 4"/>')
    out.append(f'<text x="{W - RIGHT}" y="{y - 8:.1f}" font-size="12" fill="{t["ink2"]}" text-anchor="end">{ALL_RESIDENT}, every sandbox in memory</text>')

    out.append(f'<path d="{curve}" fill="none" stroke="{t["line"]}" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round"/>')
    ex, ey = pts[-1]
    out.append(f'<circle cx="{ex:.1f}" cy="{ey:.1f}" r="5" fill="{t["line"]}" stroke="{t["surface"]}" stroke-width="2"/>')

    body = "\n".join(out)
    return f"""<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {H}" width="{W}" height="{H}" font-family='{FONT}' role="img" aria-label="{peak:,} sandboxes running on six 2 GB machines that hold {ALL_RESIDENT} kept in memory">
{body}
</svg>
"""


def main():
    src, dst = Path(sys.argv[1]), Path(sys.argv[2])
    dst.mkdir(parents=True, exist_ok=True)
    rows = list(csv.DictReader(open(src / "running.csv")))
    for theme in THEMES:
        (dst / f"fleet-running-{theme}.svg").write_text(running(rows, theme))
    print(f"wrote 2 charts to {dst}")


if __name__ == "__main__":
    main()
