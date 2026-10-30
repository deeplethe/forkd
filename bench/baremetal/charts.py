#!/usr/bin/env python3
"""Draw the comparison chart from a results directory.

    python3 bench/baremetal/charts.py bench/baremetal/results/2026-10-30 docs/assets/charts

The same drawing code and palette as bench/fleet/charts.py, and the same
rule: every chart in `-light.svg` and `-dark.svg`, and a chart whose data
is missing is skipped, not faked. Each runtime's figure is its last row in
the directory, so a re-run supersedes the one before it.
"""

import gzip
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "fleet"))
from charts import THEMES, W, Chart  # noqa: E402

# Shown in this order, top to bottom; Sandstorm's rows in bold.
ROWS = [
    ("sandstorm-fc-warm", "Sandstorm, Firecracker, warm", True),
    ("cubesandbox", "CubeSandbox", False),
    ("sandstorm-gvisor-warm", "Sandstorm, gVisor, warm", True),
    ("sandstorm-gvisor", "Sandstorm, gVisor", True),
    ("sandstorm-fc", "Sandstorm, Firecracker, cold", True),
    ("cubesandbox-cold", "CubeSandbox, cold*", False),
    ("process", "Plain process, no isolation", False),
    ("bwrap", "bubblewrap", False),
    ("podman", "Podman (crun)", False),
    ("runc", "Docker (runc)", False),
    ("runsc", "Docker + gVisor", False),
    ("microsandbox", "microsandbox", False),
    ("kata", "Kata (QEMU)", False),
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


def fleet_figures(src):
    """From this round's fleet run: (wake from disk ms, web kept in memory
    per node, % refused at 8 req/s per node)."""
    d = src.parent.parent.parent / "fleet" / "results" / src.name / "v13"
    read = lambda g: [json.loads(l) for f in d.glob(g) for l in gzip.open(f, "rt") if l.strip()]
    wakes = sorted(r["latency_s"] * 1000 for r in read("*/sleep-*.jsonl.gz")
                   if r.get("kind") == "wake" and r["status"] == 200 and r["slept_s"] >= 80)
    req = [r for r in read("*/mixed40-*.jsonl.gz") if r.get("kind") == "req" and r["rate"] == 8]
    ups = [json.load(open(f))["settled"]["up"] / 6 for f in d.glob("walls/wall_web-*.json")]
    if not wakes or not req or not ups:
        return None
    return wakes[len(wakes) // 2], sum(ups) / len(ups), 100 * sum(r["status"] != 200 for r in req) / len(req)


def tile(c, x, y, w, value, label, sub):
    t = c.t
    c.out.append(f'<rect x="{x:.1f}" y="{y}" width="{w:.1f}" height="74" rx="8" fill="{t["band"]}"/>')
    c.text(x + 12, y + 30, value, size=22, colour=t["ink"], weight=600)
    c.text(x + 12, y + 48, label, size=11, colour=t["ink2"])
    c.text(x + 12, y + 63, sub, size=11)


def hbars(c, x0, x1, y0, items, lo, hi, fmt, ticks, blank=None):
    """Log-scale horizontal bars between x0 and x1, one per row; `blank` says
    why a row has none."""
    import math
    t = c.t
    a, b = math.log10(lo), math.log10(hi)
    sx = lambda v: x0 + (x1 - x0) * (math.log10(max(v, lo)) - a) / (b - a)
    yb = y0 + 34 * len(items)
    c.out.append(f'<line x1="{x0:.1f}" x2="{x0:.1f}" y1="{y0}" y2="{yb}" stroke="{t["axis"]}"/>')
    for v in ticks:
        X = sx(v)
        c.out.append(f'<line x1="{X:.1f}" x2="{X:.1f}" y1="{y0}" y2="{yb}" stroke="{t["grid"]}"/>')
        c.text(X, yb + 18, fmt(v), anchor="middle")
    for i, (v, colour) in enumerate(items):
        y = y0 + 8 + i * 34
        if v is None:
            if blank:
                c.text(x0 + 6, y + 16, blank)
            continue
        w = sx(v) - x0
        c.out.append(f'<rect x="{x0}" y="{y + 3}" width="{w:.1f}" height="18" rx="4" fill="{colour}"/>')
        c.text(sx(v) + 6, y + 16, fmt(v), colour=t["ink"], weight=600)


MS = lambda v: f"{v / 1000:.1f} s" if v >= 1000 else f"{v:.0f} ms"


def compare(src, theme):
    """The figure at the top of the README: the headline numbers, the 64 MiB
    app everywhere it was run, one row per runtime (its start on a cloud VM
    and on the KVM machine, and its memory), and cloning. Separate plots, not
    axes on one: they are different measures."""
    kvm = summaries(src)
    cloud, nodes = fleet_summaries(src)
    pvm, _ = fleet_summaries(src, "pvm")
    cl, fig = clones(src), fleet_figures(src)
    if not nodes or len(kvm) < 2 or not fig or "sandstorm-fc" not in cl:
        return None
    wake, kept, refused = fig
    t = THEMES[theme]
    app = lambda d, k: d.get(k, {}).get("app64", (None, None))
    shown = [(k, name, ours) for k, name, ours in ROWS if app(kvm, k)[0]]
    colour = lambda k, ours: (t["series"][0] if k.startswith("sandstorm-fc") else t["series"][2] if ours
                              else t["series"][1] if k.startswith("cube") else t["muted"])
    picks = [(k, n) for k, n in CLONES if k in cl]
    h = 112 + 90 + 34 * len(shown) + 84 + 34 * len(picks) + 66
    c = Chart(theme, "Start, memory and clones, against the other sandboxes",
              "The 64 MiB app. p50, started one at a time. Log scales.",
              h=h, left=24, right=24, title_left=24, top_pad=90)
    tw = (W - 48 - 4 * 10) / 5
    fc, cube = app(kvm, "sandstorm-fc-warm")[0], app(kvm, "cubesandbox")[0]
    tiles = [(MS(fc), "warm start, KVM", f"CubeSandbox {MS(cube)}"),
             (MS(cl["sandstorm-fc"][0]), "snapshot, KVM", f"CubeSandbox {MS(cl['cubesandbox'][0])}"),
             (MS(wake), "wake from disk", "gVisor, 2 GB node"),
             (f"{kept:.0f}", "web servers in memory", "per 2 GB node"),
             (f"{refused:.1f}%", "refused at 8 req/s", "40 sandboxes per node")]
    for i, (v, a, b) in enumerate(tiles):
        tile(c, 24 + i * (tw + 10), 80, tw, v, a, b)
    y0 = c.top + 16
    lx, gap, tail = 208, 20, 58
    pw = (W - 24 - lx - 2 * (gap + tail) - tail) / 3
    name = lambda i, s, ours, base: c.text(lx - 12, base + 8 + i * 34 + 15, s, colour=t["ink"] if ours else t["ink2"],
                                           anchor="end", weight=600 if ours else 400)
    for i, (k, n, ours) in enumerate(shown):
        name(i, n, ours, y0)
    at = lambda n: lx + n * (pw + gap + tail)
    panels = [("Start, cloud VM", lambda k: app(cloud, k)[0] or app(pvm, k)[0], (30, 5000), MS, [100, 1000], "not run"),
              ("Start, KVM machine", lambda k: app(kvm, k)[0], (30, 5000), MS, [100, 1000], None),
              ("Memory, KVM machine", lambda k: app(kvm, k)[1], (5, 500), lambda v: f"{v:.0f} MiB", [10, 100], None)]
    for n, (head, get, (lo, hi), fmt, ticks, blank) in enumerate(panels):
        c.text(at(n), y0 - 6, head, colour=t["ink2"], weight=600)
        hbars(c, at(n), at(n) + pw, y0 + 4, [(get(k), colour(k, o)) for k, _, o in shown], lo, hi, fmt, ticks, blank)
    y1 = y0 + 34 * len(shown) + 64
    c.text(24, y1 - 16, "Cloud VM: 2 cores, 2 GB, gVisor and containers; Firecracker and CubeSandbox under PVM, 2 cores, 3.6 GB. KVM: 2 × Xeon E5-2637 v2, 31 GB.", size=11)
    c.text(24, y1 - 2, "* Booting a template, node side, to within 0.5 s. CubeSandbox has no cold start for a sandbox.", size=11)
    y2 = y1 + 40
    for i, (k, n) in enumerate(picks):
        name(i, n, k.startswith("sandstorm"), y2)
    for m, count in enumerate((1, 10, 20)):
        c.text(at(m), y2 - 6, f"Clone a running one into {count}", colour=t["ink2"], weight=600)
        hbars(c, at(m), at(m) + pw, y2 + 4, [(cl[k][1][m], colour(k, k.startswith("sandstorm"))) for k, _ in picks],
              100, 5000, MS, [1000])
    return c.svg("Headline numbers, then start time and memory of a 64 MiB app and the time to clone one, by runtime")


CHARTS = {"compare": compare}


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
