#!/usr/bin/env python3
"""Draw the bare-metal comparison charts from a results directory.

    python3 bench/baremetal/charts.py bench/baremetal/results/2026-10-30 docs/assets/charts

The same drawing code and palette as bench/fleet/charts.py, and the same
rule: every chart in `-light.svg` and `-dark.svg`, and a chart whose data
is missing is skipped, not faked. Each runtime's figure is its last row in
the directory, so a re-run supersedes the one before it.
"""

import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "fleet"))
from charts import THEMES, W, Chart  # noqa: E402

# Shown in this order, top to bottom; Sandstorm's rows in bold.
STARTS = [
    ("sandstorm-fc-warm", "Sandstorm, Firecracker, warm", True),
    ("cubesandbox", "CubeSandbox", False),
    ("sandstorm-gvisor-warm", "Sandstorm, gVisor, warm", True),
    ("sandstorm-gvisor", "Sandstorm, gVisor", True),
    ("runc", "Docker (runc)", False),
    ("runsc", "Docker + gVisor", False),
    ("microsandbox", "microsandbox", False),
    ("kata", "Kata (QEMU)", False),
]
# The fleet's 2-core, 2 GB nodes: no KVM, so gVisor and containers only.
FLEET_STARTS = [
    ("sandstorm-gvisor-warm", "Sandstorm, warm template", True),
    ("sandstorm-gvisor", "Sandstorm", True),
    ("process", "Plain process, no isolation", False),
    ("bwrap", "bubblewrap", False),
    ("podman", "Podman (crun)", False),
    ("runc", "Docker (runc)", False),
    ("runsc", "Docker + gVisor", False),
]
CLONES = [
    ("sandstorm-fc", "Sandstorm, Firecracker"),
    ("cubesandbox", "CubeSandbox"),
    ("microsandbox", "microsandbox"),
    ("sandstorm-gvisor", "Sandstorm, gVisor"),
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


def fleet_summaries(src):
    """The same, the median across nodes: one file per node in `fleet/`."""
    per = {}
    for f in sorted((src / "fleet").glob("*.jsonl")):
        for rt, apps in summaries_of(f).items():
            for app, (st, mem) in apps.items():
                per.setdefault(rt, {}).setdefault(app, ([], []))
                if st:
                    per[rt][app][0].append(st)
                if mem:
                    per[rt][app][1].append(mem)
    med = lambda xs: sorted(xs)[len(xs) // 2] if len(xs) % 2 else sum(sorted(xs)[len(xs) // 2 - 1:len(xs) // 2 + 1]) / 2 if xs else None
    return {rt: {app: (med(a), med(b)) for app, (a, b) in apps.items()} for rt, apps in per.items()}, \
        len(list((src / "fleet").glob("*.jsonl")))


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
    """runtime -> [clone 1, 10, 20] in ms, until every child answers. For
    Sandstorm a branch then one fork call, so the snapshot plus the fork."""
    out = {}
    for r in rows(src):
        if r.get("kind") != "fork":
            continue
        if "clone1_ms" in r:
            out[r["runtime"]] = [r["clone1_ms"], r["clone10_wall_ms"], r["clone20_wall_ms"]]
        else:
            s = r.get("snapshot_ms", 0)
            out[r["runtime"]] = [s + r["fork1_ms"], s + r["fork10_wall_ms"], s + r["fork20_wall_ms"]]
    return out


def hbars(c, x0, x1, y0, items, lo, hi, fmt, ticks):
    """Log-scale horizontal bars between x0 and x1, one per row."""
    import math
    t = c.t
    a, b = math.log10(lo), math.log10(hi)
    sx = lambda v: x0 + (x1 - x0) * (math.log10(max(v, lo)) - a) / (b - a)
    yb = y0 + 34 * len(items)
    for v in ticks:
        X = sx(v)
        c.out.append(f'<line x1="{X:.1f}" x2="{X:.1f}" y1="{y0}" y2="{yb}" stroke="{t["axis"] if v == ticks[0] else t["grid"]}"/>')
        c.text(X, yb + 18, fmt(v), anchor="middle")
    for i, (v, colour) in enumerate(items):
        if v is None:
            continue
        y = y0 + 8 + i * 34
        w = sx(v) - x0
        c.out.append(f'<rect x="{x0}" y="{y + 3}" width="{w:.1f}" height="18" rx="4" fill="{colour}"/>')
        c.text(sx(v) + 6, y + 16, fmt(v), colour=t["ink"], weight=600)


def start(src, theme):
    """Start to first answer and memory per instance for app64, side by
    side, one row per runtime. Two plots, not two axes on one: they are
    different measures."""
    return start_chart(summaries(src), STARTS, theme, "The 64 MiB app on one KVM machine",
                       "2 × Xeon E5-2637 v2, 31 GB. Start: p50 of 20, one at a time. Memory: 100 kept running. Log scales.",
                       "Start time and memory per instance of a 64 MiB app, by runtime, on one KVM machine")


def fleet(src, theme):
    """The same chart on the fleet's 2-core, 2 GB nodes, the median node."""
    s, nodes = fleet_summaries(src)
    if not nodes:
        return None
    return start_chart(s, FLEET_STARTS, theme, "The 64 MiB app on 2-core, 2 GB cloud machines",
                       f"Median of {nodes} nodes, no KVM. Start: p50 of 20, one at a time. Memory: 8 kept running. Log scales.",
                       "Start time and memory per instance of a 64 MiB app, by runtime, on 2-core, 2 GB machines")


def start_chart(s, order, theme, title, subtitle, label):
    shown = [(k, name, ours) for k, name, ours in order if "app64" in s.get(k, {})]
    if len(shown) < 2:
        return None
    t = THEMES[theme]
    colour = lambda k, ours: t["series"][0] if k == "sandstorm-fc-warm" else t["series"][2] if ours else t["series"][1] if k == "cubesandbox" else t["muted"]
    h = 112 + 34 * len(shown) + 40
    c = Chart(theme, title, subtitle, h=h, left=24, right=24, title_left=24)
    y0 = c.top + 16
    lx, gap = 236, 28
    mid = lx + (W - 24 - lx - gap) / 2
    c.text(lx, y0 - 6, "Start to first answer", colour=t["ink2"], weight=600)
    c.text(mid + gap, y0 - 6, "Memory per instance", colour=t["ink2"], weight=600)
    for i, (k, name, ours) in enumerate(shown):
        c.text(lx - 12, y0 + 8 + i * 34 + 15, name, colour=t["ink"] if ours else t["ink2"], anchor="end", weight=600 if ours else 400)
    ms = lambda v: f"{v / 1000:.1f} s" if v >= 1000 else f"{v:.0f} ms"
    hbars(c, lx, mid - 40, y0 + 4, [(s[k]["app64"][0], colour(k, o)) for k, _, o in shown],
          30, 10000, ms, [30, 100, 1000, 10000])
    hbars(c, mid + gap, W - 24 - 72, y0 + 4, [(s[k]["app64"][1], colour(k, o)) for k, _, o in shown],
          5, 500, lambda v: f"{v:.0f}" + (" MiB" if v >= 300 else ""), [5, 10, 100, 500])
    return c.svg(label)


def clone(src, theme):
    """Copying a running app64 into n that answer: grouped bars by n."""
    cl = clones(src)
    shown = [(k, label) for k, label in CLONES if k in cl]
    if len(shown) < 2:
        return None
    t = THEMES[theme]
    colours = {"sandstorm-fc": t["series"][0], "cubesandbox": t["series"][1],
               "sandstorm-gvisor": t["series"][2], "microsandbox": t["muted"]}
    c = Chart(theme, "Cloning a running sandbox",
              "Snapshot a running 64 MiB app, then start n copies; until all answer (microsandbox: until running).",
              h=360, left=56, right=24, title_left=24,
              legend=[(colours[k], label, "dot") for k, label in shown])
    x0, y0, x1, y1 = c.plot
    c.yscale(0, 2500)
    c.ygrid([0, 500, 1000, 1500, 2000, 2500], lambda v: f"{v / 1000:g} s" if v else "0")
    slot = (x1 - x0) / 3
    bw = min(26, slot / (len(shown) + 1.5))
    for i, n in enumerate((1, 10, 20)):
        cx = x0 + slot * (i + 0.5)
        for j, (k, _) in enumerate(shown):
            v = cl[k][i]
            bx = cx + (j - (len(shown) - 1) / 2) * (bw + 4)
            yt = c.sy(v)
            r = min(4, y1 - yt)
            c.out.append(f'<path d="M{bx - bw / 2:.1f},{y1} V{yt + r:.1f} Q{bx - bw / 2:.1f},{yt:.1f} {bx - bw / 2 + r:.1f},{yt:.1f} H{bx + bw / 2 - r:.1f} Q{bx + bw / 2:.1f},{yt:.1f} {bx + bw / 2:.1f},{yt + r:.1f} V{y1} Z" fill="{colours[k]}"/>')
            c.text(bx, yt - 6, f"{v:,.0f}", size=10, colour=t["ink"], anchor="middle", weight=600)
        c.text(cx, y1 + 22, f"n = {n}", anchor="middle")
    return c.svg("Time to clone a running sandbox into 1, 10 and 20 copies, by runtime")


CHARTS = {"baremetal-start": start, "baremetal-clone": clone, "fleet-start": fleet}


def main():
    src, dst = Path(sys.argv[1]), Path(sys.argv[2])
    dst.mkdir(parents=True, exist_ok=True)
    wrote = []
    for name, draw in CHARTS.items():
        for theme in THEMES:
            svg = draw(src, theme)
            if svg is None:
                break
            (dst / f"{name}-{theme}.svg").write_text(svg)
        else:
            wrote.append(name)
    print(f"wrote {', '.join(wrote) or 'nothing'} to {dst}")


if __name__ == "__main__":
    main()
