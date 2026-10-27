#!/usr/bin/env python3
"""Draw the fleet charts from a results directory.

    python3 bench/fleet/charts.py bench/fleet/results/2026-10-26 docs/assets/charts

Every chart is written twice, `-light.svg` and `-dark.svg`, for a
`<picture>` to choose between. Stdlib only, like loadtest.py: the SVG is
written by hand, so redrawing needs nothing installed. A chart whose data
is missing from the directory is skipped, not faked.
"""

import csv
import json
import math
import sys
from pathlib import Path

FONT = 'system-ui, -apple-system, "Segoe UI", Helvetica, Arial, sans-serif'

# Categorical slots 1-3 of the dataviz reference palette, stepped for each
# surface; validated adjacent-pair CVD dE >= 9.2 in both modes.
THEMES = {
    "light": {
        "surface": "#fcfcfb", "ink": "#0b0b0b", "ink2": "#52514e", "muted": "#898781",
        "grid": "#ecebe6", "axis": "#c3c2b7", "band": "#f3f2ee",
        "series": ["#2a78d6", "#eb6834", "#1baf7a"],
    },
    "dark": {
        "surface": "#1a1a19", "ink": "#f0efec", "ink2": "#c3c2b7", "muted": "#898781",
        "grid": "#262624", "axis": "#383835", "band": "#222220",
        "series": ["#3987e5", "#d95926", "#199e70"],
    },
}

W = 800


# --- drawing primitives -----------------------------------------------------

class Chart:
    """A titled plot area with one x scale and one y scale."""

    def __init__(self, theme, title, subtitle, h=400, legend=(), top_pad=0, left=60, right=32, bottom=44):
        self.t = THEMES[theme]
        self.h = h
        self.left, self.right, self.bottom = left, right, bottom
        self.top = 96 + (22 if legend else 0) + top_pad
        self.out = [f'<rect width="{W}" height="{h}" rx="12" fill="{self.t["surface"]}"/>']
        self.text(self.left, 40, title, size=18, weight=600, colour=self.t["ink"])
        self.text(self.left, 64, subtitle, size=13, colour=self.t["ink2"])
        x, ly = self.left, 92
        for colour, label, kind in legend:
            if x + 30 + 7 * len(label) > W - self.right:
                x, ly = self.left, ly + 20
                self.top += 20
            if kind == "line":
                self.out.append(f'<line x1="{x}" x2="{x + 16}" y1="{ly}" y2="{ly}" stroke="{colour}" stroke-width="2.5" stroke-linecap="round"/>')
                x += 22
            else:
                self.out.append(f'<circle cx="{x + 5}" cy="{ly}" r="4.5" fill="{colour}"/>')
                x += 16
            self.text(x, ly + 4, label, size=12, colour=self.t["ink2"])
            x += 7 * len(label) + 22

    @property
    def plot(self):
        return self.left, self.top, W - self.right, self.h - self.bottom

    def text(self, x, y, s, size=12, colour=None, anchor="start", weight=400):
        self.out.append(
            f'<text x="{x:.1f}" y="{y:.1f}" font-size="{size}" font-weight="{weight}" '
            f'fill="{colour or self.t["muted"]}" text-anchor="{anchor}">{s}</text>'
        )

    def xscale(self, lo, hi, log=False):
        x0, _, x1, _ = self.plot
        if log:
            a, b = math.log10(lo), math.log10(hi)
            self.sx = lambda v: x0 + (x1 - x0) * (math.log10(max(v, lo)) - a) / (b - a)
        else:
            self.sx = lambda v: x0 + (x1 - x0) * (v - lo) / (hi - lo)

    def yscale(self, lo, hi, log=False):
        _, y0, _, y1 = self.plot
        if log:
            a, b = math.log10(lo), math.log10(hi)
            self.sy = lambda v: y1 - (y1 - y0) * (math.log10(min(max(v, lo), hi)) - a) / (b - a)
        else:
            self.sy = lambda v: y1 - (y1 - y0) * (v - lo) / (hi - lo)

    def ygrid(self, ticks, fmt):
        x0, _, x1, _ = self.plot
        for i, v in enumerate(ticks):
            y = self.sy(v)
            self.out.append(f'<line x1="{x0}" x2="{x1}" y1="{y:.1f}" y2="{y:.1f}" stroke="{self.t["axis"] if i == 0 else self.t["grid"]}"/>')
            self.text(x0 - 10, y + 4, fmt(v), anchor="end")

    def xticks(self, ticks, fmt):
        _, _, _, y1 = self.plot
        for v in ticks:
            self.text(self.sx(v), y1 + 24, fmt(v), anchor="middle")

    def band(self, xa, xb, label=None):
        _, y0, _, y1 = self.plot
        a, b = self.sx(xa), self.sx(xb)
        self.out.append(f'<rect x="{a:.1f}" y="{y0}" width="{b - a:.1f}" height="{y1 - y0}" fill="{self.t["band"]}"/>')
        if label:
            self.text((a + b) / 2, y0 + 14, label, size=11, anchor="middle")

    def vline(self, x, label=None):
        _, y0, _, y1 = self.plot
        X = self.sx(x)
        self.out.append(f'<line x1="{X:.1f}" x2="{X:.1f}" y1="{y0}" y2="{y1}" stroke="{self.t["axis"]}"/>')
        if label:
            self.text(X + 6, y0 + 14, label, size=11)

    def hline(self, y, label):
        x0, _, x1, _ = self.plot
        Y = self.sy(y)
        self.out.append(f'<line x1="{x0}" x2="{x1}" y1="{Y:.1f}" y2="{Y:.1f}" stroke="{self.t["ink2"]}" stroke-width="1.25" stroke-dasharray="4 4"/>')
        self.text(x1, Y - 8, label, colour=self.t["ink2"], anchor="end")

    def line(self, pts, colour, width=2.5, smooth=True, fill=False, dot_end=True):
        P = [(self.sx(x), self.sy(y)) for x, y in pts]
        d = monotone(P) if smooth and len(P) > 2 else "M" + " L".join(f"{x:.1f},{y:.1f}" for x, y in P)
        if fill:
            _, _, _, y1 = self.plot
            gid = f"g{len(self.out)}"
            self.out.append(
                f'<defs><linearGradient id="{gid}" x1="0" y1="0" x2="0" y2="1">'
                f'<stop offset="0" stop-color="{colour}" stop-opacity="0.18"/>'
                f'<stop offset="1" stop-color="{colour}" stop-opacity="0"/></linearGradient></defs>'
            )
            self.out.append(f'<path d="{d} L{P[-1][0]:.1f},{y1} L{P[0][0]:.1f},{y1} Z" fill="url(#{gid})"/>')
        self.out.append(f'<path d="{d}" fill="none" stroke="{colour}" stroke-width="{width}" stroke-linecap="round" stroke-linejoin="round"/>')
        if dot_end:
            self.dot(*pts[-1], colour)
        return P

    def dot(self, x, y, colour, r=4.5, opacity=1.0, ring=True):
        ring_attr = f' stroke="{self.t["surface"]}" stroke-width="2"' if ring else ""
        self.out.append(f'<circle cx="{self.sx(x):.1f}" cy="{self.sy(y):.1f}" r="{r}" fill="{colour}" fill-opacity="{opacity}"{ring_attr}/>')

    def svg(self, label):
        body = "\n".join(self.out)
        return (
            f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {self.h}" width="{W}" height="{self.h}" '
            f"font-family='{FONT}' role=\"img\" aria-label=\"{label}\">\n{body}\n</svg>\n"
        )


def monotone(points):
    """An SVG path through `points` that never overshoots them.

    Fritsch-Carlson monotone cubic: smooth, but a value that holds flat is
    drawn flat, and the curve never passes a value the data did not have.
    """
    n = len(points)
    xs = [p[0] for p in points]
    ys = [p[1] for p in points]
    d = [(ys[i + 1] - ys[i]) / ((xs[i + 1] - xs[i]) or 1e-9) for i in range(n - 1)]
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


def quantile(xs, q):
    xs = sorted(xs)
    if not xs:
        return None
    k = (len(xs) - 1) * q
    f = math.floor(k)
    c = min(f + 1, len(xs) - 1)
    return xs[f] + (xs[c] - xs[f]) * (k - f)


def ms(v):
    return f"{v:g} ms" if v < 1000 else f"{v / 1000:g} s"


def jsonl(path):
    return [json.loads(l) for l in open(path)] if path.exists() else []


# --- the charts ---------------------------------------------------------------

def running(src, theme):
    """The running count over a ramp: before #351, after it, and after the FD fix."""
    t = THEMES[theme]

    def load(name):
        p = src / name
        return [(int(r["seconds"]), int(r["running"])) for r in csv.DictReader(open(p))] if p.exists() else []

    runs = [
        (load("running-r3b.csv"), t["series"][0], "snapshots release their netns, open-file limit raised"),
        (load("running.csv"), t["series"][1], "before: each snapshot kept its netns"),
        (load("running-r3a.csv"), t["series"][2], "netns released, open-file limit left at 1024"),
    ]
    runs = [r for r in runs if r[0]]
    peak = max(max(v for _, v in pts) for pts, _, _ in runs)
    ymax = math.ceil(peak * 1.12 / 4000) * 4000
    xmax = 19 * 60
    c = Chart(theme, f"{peak:,} sandboxes running on six 2-core, 2 GB machines",
              "Idle Python sandboxes created at 15-20/s. The dashed line is what the same machines hold kept in memory.",
              legend=[(colour, label, "line") for _, colour, label in runs], h=430)
    # Three legend entries do not fit on one row; give the third its own.
    c.xscale(0, xmax)
    c.yscale(0, ymax)
    c.ygrid(range(0, ymax + 1, 4000), lambda v: "0" if v == 0 else f"{v // 1000}k")
    c.xticks(range(0, xmax + 1, 180), lambda s: f"{s // 60} min")
    c.hline(431, "431, every sandbox in memory")
    for pts, colour, _ in reversed(runs):
        pts = [(x, y) for x, y in pts if x <= xmax]
        c.line(pts, colour, width=2.5 if colour == t["series"][0] else 2, fill=colour == t["series"][0])
        x, y = pts[-1]
        below = colour == t["series"][2]
        c.text(c.sx(x) - 8, c.sy(y) + (20 if below else -10), f"{max(v for _, v in pts):,}", size=12, colour=t["ink"], anchor="end", weight=600)
    return c.svg(f"{peak:,} sandboxes running on six 2-core, 2 GB machines")


def sleep(src, theme):
    rows = [r for r in jsonl(src / "r3" / "sleep.jsonl") if r["kind"] == "wake" and r["status"] == 200]
    if not rows:
        return None
    t = THEMES[theme]
    frozen, snap = 15, 60
    c = Chart(theme, "How long a request waits, by how long its sandbox slept",
              f"app64 (Python, numpy, 64 MiB heap). Frozen after {frozen} s idle, on disk after {snap} s. Timed on the node: no network.",
              legend=[(t["series"][1], "the wake itself (sandstorm)", "line"), (t["series"][0], "the whole answer", "line")])
    xmax = max(r["slept_s"] for r in rows) * 1.05
    lo, hi = 1, 1000
    c.xscale(0, xmax)
    c.yscale(lo, hi, log=True)
    c.band(0, frozen, "hot")
    c.band(snap, xmax, "on disk")
    c.text((c.sx(frozen) + c.sx(snap)) / 2, c.plot[1] + 14, "frozen", size=11, anchor="middle")
    c.ygrid([1, 10, 100, 1000], ms)
    c.xticks([s for s in (0, 15, 30, 60, 90, 120, 150, 180) if s <= xmax], lambda s: f"{s} s")
    ages = sorted({r["slept_s"] for r in rows})
    whole = [(a, quantile([r["latency_s"] * 1000 for r in rows if r["slept_s"] == a], 0.5)) for a in ages]
    # The gateway reports how long it spent waking (x-sandstorm-woke-ms); a
    # request that found its sandbox hot did not wait for a wake at all.
    woke = [(a, quantile([r["woke_ms"] or 0 for r in rows if r["slept_s"] == a], 0.5)) for a in ages]
    woke = [(a, v) for a, v in woke if v and v >= 1]
    for r in rows:
        c.dot(r["slept_s"], r["latency_s"] * 1000, t["series"][0], r=3, opacity=0.3, ring=False)
    c.line(whole, t["series"][0], smooth=False)
    c.line(woke, t["series"][1], smooth=False)
    for pts, colour, above in ((whole, t["ink"], True), (woke, t["ink2"], False)):
        a, v = pts[-1]
        c.text(c.sx(a) - 8, c.sy(v) + (-12 if above else 20), ms(round(v)), size=12, colour=colour, anchor="end", weight=600)
    fz = [v for a, v in woke if frozen < a < snap]
    if fz:
        mid = (c.sx(frozen) + c.sx(snap)) / 2
        thaw = round(quantile(fz, 0.5))
        c.text(mid, c.sy(quantile(fz, 0.5)) + 22, f"thaw: {thaw} ms", size=11, colour=t["ink2"], anchor="middle")
        c.text(mid, c.sy(quantile(fz, 0.5)) + 37, "the rest: heap back from zswap", size=11, colour=t["ink2"], anchor="middle")
    return c.svg("Wake latency by idle time")


def herd(src, theme):
    rows = [r for r in jsonl(src / "r3" / "herd.jsonl") if r["kind"] == "herd"]
    if not rows:
        return None
    t = THEMES[theme]
    by = {}
    for r in rows:
        by.setdefault(r["size"] if r["size"] < 30 else 40, []).append(r)
    sizes = sorted(by)
    # Each node woke N of its own at once; the slowest answer on any node.
    wall = [(k, max(r["wall_s"] for r in by[k])) for k in sizes]
    p50 = [(k, quantile([x for r in by[k] for x in r["latencies_s"]], 0.5)) for k in sizes]
    woke = {k: (sum(r["ok"] for r in by[k]), sum(r["size"] for r in by[k])) for k in sizes}
    c = Chart(theme, "Waking many at once",
              f"Each of {len(by[sizes[0]])} nodes wakes N app64 snapshots at once; past what fits in memory, it refuses the rest.",
              legend=[(t["series"][0], "median answer", "line"), (t["series"][1], "slowest answer", "line")])
    ymax = math.ceil(max(v for _, v in wall) * 1.25)
    c.xscale(0, sizes[-1])
    c.yscale(0, ymax)
    c.ygrid(range(0, ymax + 1, 1), lambda v: f"{v} s")
    c.xticks(sizes, lambda k: f"{k}")
    c.text(W - c.right, c.h - 6, "woken at once, per node", anchor="end", size=11)
    c.line(wall, t["series"][1], smooth=False)
    c.line(p50, t["series"][0], smooth=False)
    for k, v in wall + p50:
        c.dot(k, v, t["series"][1] if (k, v) in wall else t["series"][0], r=4)
    for k, v in wall:
        ok, asked = woke[k]
        c.text(c.sx(k), c.sy(v) - 14, f"{ok / asked:.0%} woke", size=11, colour=t["ink2"], anchor="end" if k == sizes[-1] else "middle")
    return c.svg("Herd wake latency")


def mixed(src, theme):
    rows = [r for r in jsonl(src / "r3" / "mixed.jsonl") if r["kind"] == "req"]
    if not rows:
        return None
    t = THEMES[theme]
    # Nodes started their traffic a few seconds apart: time each from its own start.
    start = {}
    for r in rows:
        start[r["node"]] = min(start.get(r["node"], 1e18), r["t"] - r["latency_s"])
    for r in rows:
        r["rel"] = r["t"] - r["latency_s"] - start[r["node"]]
    nodes = len(start)
    win = 15
    buckets = {}
    for r in rows:
        if r["status"] == 200:
            buckets.setdefault(int(r["rel"] // win), []).append(r["latency_s"] * 1000)
    keys = sorted(buckets)
    p50 = [((k + 0.5) * win, quantile(buckets[k], 0.5)) for k in keys if len(buckets[k]) >= 5]
    p99 = [((k + 0.5) * win, quantile(buckets[k], 0.99)) for k in keys if len(buckets[k]) >= 5]
    ok = sum(r["status"] == 200 for r in rows)
    pop = 40 * nodes
    c = Chart(theme, "40 sandboxes per node, where about 12 fit awake",
              f"40 app64 per node, Zipf popularity, open-loop arrivals at a rising rate. {len(rows):,} requests, {ok / len(rows):.0%} answered.",
              legend=[(t["series"][0], "median answer", "line"), (t["series"][1], "p99 answer", "line")], top_pad=18)
    xmax = (keys[-1] + 1) * win
    hi = 10 ** math.ceil(math.log10(max(v for _, v in p99)))
    c.xscale(0, xmax)
    c.yscale(1, hi, log=True)
    phases = sorted({r["rate"] for r in rows})
    starts = [min(r["rel"] for r in rows if r["rate"] == p) for p in phases]
    for i, (p, s0) in enumerate(zip(phases, starts)):
        e = starts[i + 1] if i + 1 < len(starts) else xmax
        if i % 2 == 1:
            c.band(s0, e)
        mine = [r for r in rows if r["rate"] == p]
        refused = sum(r["status"] != 200 for r in mine) / len(mine)
        mid = (c.sx(s0) + c.sx(e)) / 2
        c.text(mid, c.plot[1] - 6, f"{p * nodes:g} req/s", size=11, colour=t["ink2"], anchor="middle", weight=600)
        c.text(mid, c.plot[1] + 14, f"{refused:.0%} refused", size=11, anchor="middle")
    c.ygrid([10 ** k for k in range(0, int(math.log10(hi)) + 1)], ms)
    c.xticks(range(0, int(xmax) + 1, 120), lambda s: f"{s // 60} min")
    c.line(p99, t["series"][1], width=2, smooth=False, dot_end=False)
    c.line(p50, t["series"][0], smooth=False, dot_end=False)
    return c.svg("Latency under mixed traffic")


def sizes(src, theme):
    heaps = [16, 64, 256, 512, 1024]
    # In a gVisor sandbox, kept in memory: the hot walls for the three
    # smaller apps (six nodes, so per node is the fleet figure over six);
    # for 512 MiB and 1 GiB, the 2 and 1 per node that ran, a number more
    # than memory would take.
    sandboxed = {}
    for h in (16, 64, 256):
        p = src / "r3" / f"wall_app{h}.json"
        if p.exists():
            sandboxed[h] = json.load(open(p))["settled"]["up"] / 6
    heavy = jsonl(src / "r3" / "heavy.jsonl")
    for h in (512, 1024):
        ran = [r for r in heavy if r["kind"] == "first_request" and r["template"] == f"app{h}" and r["status"] == 200]
        if ran:
            sandboxed[h] = len(ran) / len({r["node"] for r in ran})
    # The same app as a plain process: no sandbox, no tiering. A node whose
    # memory was already under the floor when the count started reads 0;
    # those runs are dropped, not averaged in.
    bare = {}
    for r in jsonl(src / "r3" / "bare.jsonl"):
        if r.get("heap_mib") and r["processes"]:
            bare.setdefault(r["heap_mib"], []).append(r["processes"])
    restores = {}
    for r in jsonl(src / "r3" / "sizes.jsonl") + heavy:
        if r["kind"] == "restore" and r["status"] == 200:
            restores.setdefault(int(r["template"][3:]), []).append(r["latency_s"] * 1000)
    heaps = [h for h in heaps if h in sandboxed and h in restores]
    if not heaps:
        return None
    t = THEMES[theme]
    c = Chart(theme, "What a heavier sandbox costs",
              "Python with numpy and a heap from 16 MiB to 1 GiB that every request reads.",
              h=400, top_pad=24, legend=[(t["series"][1], "plain process", "dot"), (t["series"][0], "gVisor sandbox", "dot")])
    x0, y0, x1, y1 = c.plot
    gap = 36
    split = x0 + (x1 - x0) * 0.56
    panels = [(x0, split - gap / 2), (split + gap / 2, x1)]

    def bar(cx, v, vmax, colour, label, bw=16):
        bh = (y1 - y0 - 24) * v / vmax
        yt = y1 - bh
        r = min(4, bh)
        c.out.append(f'<path d="M{cx - bw / 2:.1f},{y1} V{yt + r:.1f} Q{cx - bw / 2:.1f},{yt:.1f} {cx - bw / 2 + r:.1f},{yt:.1f} H{cx + bw / 2 - r:.1f} Q{cx + bw / 2:.1f},{yt:.1f} {cx + bw / 2:.1f},{yt + r:.1f} V{y1} Z" fill="{colour}"/>')
        c.text(cx, yt - 7, label, size=11, colour=t["ink"], anchor="middle", weight=600)

    def axis(left, right, title):
        c.text(left, y0 - 12, title, size=13, colour=t["ink"], weight=600)
        c.out.append(f'<line x1="{left}" x2="{right}" y1="{y1}" y2="{y1}" stroke="{t["axis"]}"/>')

    left, right = panels[0]
    axis(left, right, "Kept in memory, per node")
    slot = (right - left) / len(heaps)
    vmax = max([sandboxed[h] for h in heaps] + [quantile(bare[h], 0.5) for h in heaps if h in bare]) * 1.2
    for i, h in enumerate(heaps):
        cx = left + slot * (i + 0.5)
        if h in bare:
            bar(cx - 9, quantile(bare[h], 0.5), vmax, t["series"][1], f"{quantile(bare[h], 0.5):.0f}")
        bar(cx + 9, sandboxed[h], vmax, t["series"][0], f"{sandboxed[h]:.0f}")
        c.text(cx, y1 + 20, f"{h} MiB" if h < 1024 else "1 GiB", anchor="middle", size=11)

    left, right = panels[1]
    axis(left, right, "Wake from disk, median")
    slot = (right - left) / len(heaps)
    med = {h: quantile(restores[h], 0.5) for h in heaps}
    vmax = max(med.values()) * 1.2
    for i, h in enumerate(heaps):
        cx = left + slot * (i + 0.5)
        v = med[h]
        bar(cx, v, vmax, t["series"][0], f"{v:.0f} ms" if v < 1000 else f"{v / 1000:.1f} s", bw=20)
        c.text(cx, y1 + 20, f"{h}" if h < 1024 else "1G", anchor="middle", size=11)
    return c.svg("Cost by heap size")


CHARTS = {"running": running, "sleep": sleep, "herd": herd, "mixed": mixed, "sizes": sizes}


def main():
    src, dst = Path(sys.argv[1]), Path(sys.argv[2])
    dst.mkdir(parents=True, exist_ok=True)
    wrote = []
    for name, draw in CHARTS.items():
        for theme in THEMES:
            svg = draw(src, theme)
            if svg is None:
                break
            (dst / f"fleet-{name}-{theme}.svg").write_text(svg)
        else:
            wrote.append(name)
    print(f"wrote {', '.join(wrote) or 'nothing'} to {dst}")


if __name__ == "__main__":
    main()
