#!/usr/bin/env python3
"""Draw the README's figure from a results directory.

    python3 bench/baremetal/charts.py bench/baremetal/results/2026-10-30 docs/assets/charts

Written as `-light.svg` and `-dark.svg`, and skipped, not faked, if its data
is missing. Each runtime's figure is its last row in
the directory, so a re-run supersedes the one before it.
"""

import json
import math
import sys
from pathlib import Path



# Each runtime with its short name; Sandstorm's bars take the dark fill.
ROWS = [
    ("sandstorm-fc-warm", "FC warm", True),
    ("sandstorm-fc", "FC cold", True),
    ("sandstorm-gvisor-warm", "gVisor warm", True),
    ("sandstorm-gvisor", "gVisor cold", True),
    ("cubesandbox", "Cube warm", False),
    ("cubesandbox-cold", "Cube cold", False),
    ("microsandbox", "microsandbox", False),
    ("kata", "Kata", False),
    ("runsc", "Docker+gVisor", False),
    ("runc", "Docker", False),
    ("podman", "Podman", False),
    ("bwrap", "bubblewrap", False),
    ("process", "Process", False),
]


def rows(src):
    for f in sorted(src.glob("*.jsonl")):
        for line in open(f):
            if line.strip():
                yield json.loads(line)


def summaries(src):
    """runtime -> app -> (start p50 ms, MiB per instance), last row wins."""
    out = {}
    for r in rows(src):
        if r.get("kind") != "summary":
            continue
        start = (r.get("start") or {}).get("p50_ms")
        mem = (r.get("memory") or {}).get("per_instance_mib")
        prev = out.setdefault(r["runtime"], {}).get(r["app"], (None, None))
        out[r["runtime"]][r["app"]] = (start or prev[0], mem or prev[1])
    return out


def fleet_summaries(src, sub="fleet"):
    """The same, the median across nodes: one file per node in `fleet/`."""
    per = {}
    for f in sorted((src / sub).glob("*.jsonl")):
        for rt, apps in summaries_of(f).items():
            for app, (st, mem) in apps.items():
                per.setdefault(rt, {}).setdefault(app, ([], []))
                if st:
                    per[rt][app][0].append(st)
                if mem:
                    per[rt][app][1].append(mem)
    med = lambda xs: sorted(xs)[len(xs) // 2] if len(xs) % 2 else sum(sorted(xs)[len(xs) // 2 - 1:len(xs) // 2 + 1]) / 2 if xs else None
    return {rt: {app: (med(a), med(b)) for app, (a, b) in apps.items()} for rt, apps in per.items()}, \
        len(list((src / sub).glob("*.jsonl")))


def summaries_of(f):
    out = {}
    for line in open(f):
        if not line.strip():
            continue
        r = json.loads(line)
        if r.get("kind") == "summary":
            out.setdefault(r["runtime"], {})[r["app"]] = (
                (r.get("start") or {}).get("p50_ms"), (r.get("memory") or {}).get("per_instance_mib"))
    return out


def clones(src):
    """runtime -> (snapshot, [clone 1, 10, 20]) in ms, until every child
    answers. For Sandstorm a branch then one fork call, so their sum."""
    out = {}
    for r in rows(src):
        if r.get("kind") != "fork":
            continue
        s = r.get("snapshot_ms", 0)
        if "clone1_ms" in r:
            out[r["runtime"]] = (s, [r["clone1_ms"], r["clone10_wall_ms"], r["clone20_wall_ms"]])
        else:
            out[r["runtime"]] = (s, [s + r["fork1_ms"], s + r["fork10_wall_ms"], s + r["fork20_wall_ms"]])
    return out


# Drawn as a figure in a paper: no card, no colour, two fills, real axes. Its
# name and notes are the README's caption. Each theme keeps the same structure.
SERIF = '"Times New Roman", Times, "Liberation Serif", "Noto Serif", serif'
INK = {
    "light": {"bg": "#ffffff", "ink": "#111111", "mid": "#555555", "ours": "#222222", "rest": "#c8c8c8"},
    "dark": {"bg": "#0d1117", "ink": "#e6e6e6", "mid": "#a0a0a0", "ours": "#e6e6e6", "rest": "#565b63"},
}
ROW, BAR, W = 21, 12, 1120


def text(o, x, y, s, fill, size=13, anchor="start", extra=""):
    o.append(f'<text x="{x:.1f}" y="{y:.1f}" font-size="{size}" fill="{fill}" text-anchor="{anchor}"{extra}>{s}</text>')


def plot(o, ink, x, y, label, width, tail, letter, head, items, lo, hi, ticks):
    """One ranked panel: labels right-aligned to the left spine, log-scale
    bars, the value at each bar's end, and an axis with decade and minor ticks."""
    a, b = math.log10(lo), math.log10(hi)
    x0 = x + label
    sx = lambda v: x0 + width * (math.log10(v) - a) / (b - a)
    text(o, x, y - 10, f'({letter})', ink["ink"], extra=' font-weight="700"')
    text(o, x + 24, y - 10, head, ink["ink"])
    y1 = y + ROW * len(items) + 3
    line = lambda x1, y_1, x2, y2: o.append(f'<line x1="{x1:.1f}" y1="{y_1:.1f}" x2="{x2:.1f}" y2="{y2:.1f}" stroke="{ink["mid"]}" stroke-width="0.8"/>')
    line(x0, y - 2, x0, y1)
    line(x0, y1, x0 + width, y1)
    for d in range(math.floor(a), math.ceil(b)):
        for m in range(1, 10):
            v = m * 10 ** d
            if lo <= v <= hi:
                line(sx(v), y1, sx(v), y1 + (5 if m == 1 else 2.5))
    for v in ticks:
        text(o, sx(v), y1 + 17, f"{v:,}", ink["mid"], size=12, anchor="middle")
    for i, (v, name, ours) in enumerate(items):
        yr = y + i * ROW
        text(o, x0 - 7, yr + 13.5, name, ink["ink"], anchor="end")
        o.append(f'<rect x="{x0:.1f}" y="{yr + 3}" width="{sx(v) - x0:.1f}" height="{BAR}" fill="{ink["ours"] if ours else ink["rest"]}"/>')
        text(o, sx(v) + 4, yr + 13.5, f"{v:,.0f}", ink["ink"], size=12)
    return y1


def compare(src, theme):
    """Figure 1 of the README: five rankings of the 64 MiB app, each in its
    own order, since one order cannot rank them all."""
    kvm = summaries(src)
    cloud, nodes = fleet_summaries(src)
    pvm, _ = fleet_summaries(src, "pvm")
    cl = clones(src)
    if not nodes or len(kvm) < 2 or "sandstorm-fc" not in cl:
        return None
    ink = INK[theme]
    app = lambda d, k: d.get(k, {}).get("app64", (None, None))
    fork = {"sandstorm-fc-warm": "sandstorm-fc", "sandstorm-gvisor-warm": "sandstorm-gvisor", "cubesandbox": "cubesandbox", "microsandbox": "microsandbox"}
    forked = lambda k, f: f(cl[fork[k]]) if k in fork and fork[k] in cl else None
    ranked = lambda get: sorted(((get(k), n, o) for k, n, o in ROWS if get(k)), key=lambda r: r[0])
    label, width, tail, gap = 100, 118, 40, 18
    col = label + width + tail + gap
    x0 = (W - 4 * col + gap) / 2
    top, o = 32, [f'<rect width="{W}" height="@H@" fill="{ink["bg"]}"/>']
    panels = [(0, top, "a", "Start, KVM machine (ms)", lambda k: app(kvm, k)[0], 30, 5000, [100, 1000]),
              (1, top, "b", "Start, cloud VM (ms)", lambda k: app(cloud, k)[0] or app(pvm, k)[0], 30, 5000, [100, 1000]),
              (2, top, "c", "Memory per instance (MiB)", lambda k: app(kvm, k)[1], 5, 500, [10, 100])]
    bottom = 0
    for c, y, letter, head, get, lo, hi, ticks in panels:
        bottom = max(bottom, plot(o, ink, x0 + c * col, y, label, width, tail, letter, head, ranked(get), lo, hi, ticks))
    y = plot(o, ink, x0 + 3 * col, top, label, width, tail, "d", "Clone into 20 (ms)", ranked(lambda k: forked(k, lambda c: c[1][2])), 100, 5000, [1000])
    plot(o, ink, x0 + 3 * col, y + 58, label, width, tail, "e", "Snapshot (ms)", ranked(lambda k: forked(k, lambda c: c[0] or None)), 10, 1000, [100])
    h = bottom + 34
    body = "\n".join(o).replace("@H@", str(h))
    return (f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {h}" width="{W}" height="{h}" role="img" '
            f"aria-label=\"Five rankings of a 64 MiB app by runtime: start, memory, clone and snapshot\" font-family='{SERIF}'>\n{body}\n</svg>\n")


CHARTS = {"compare": compare}


def main():
    src, dst = Path(sys.argv[1]), Path(sys.argv[2])
    dst.mkdir(parents=True, exist_ok=True)
    wrote = []
    for name, draw in CHARTS.items():
        for theme in INK:
            svg = draw(src, theme)
            if svg is None:
                break
            (dst / f"{name}-{theme}.svg").write_text(svg)
        else:
            wrote.append(name)
    print(f"wrote {', '.join(wrote) or 'nothing'} to {dst}")


if __name__ == "__main__":
    main()
