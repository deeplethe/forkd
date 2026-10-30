#!/usr/bin/env python3
"""Draw the comparison chart from a results directory.

    python3 bench/baremetal/charts.py bench/baremetal/results/2026-10-30 docs/assets/charts

The same drawing code and palette as bench/fleet/charts.py, and the same
rule: every chart in `-light.svg` and `-dark.svg`, and a chart whose data
is missing is skipped, not faked. Each runtime's figure is its last row in
the directory, so a re-run supersedes the one before it.
"""

import base64
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "fleet"))
from charts import THEMES, Chart  # noqa: E402

W, ROW, LOGOS = 1120, 27, Path(__file__).resolve().parent.parent.parent / "docs" / "assets" / "logos"

# Each runtime with its short name and logo; Sandstorm's bars are blue.
ROWS = [
    ("sandstorm-fc-warm", "FC warm", True, "sandstorm"),
    ("sandstorm-fc", "FC cold", True, "sandstorm"),
    ("sandstorm-gvisor-warm", "gVisor warm", True, "sandstorm"),
    ("sandstorm-gvisor", "gVisor cold", True, "sandstorm"),
    ("cubesandbox", "Cube warm", False, "cubesandbox"),
    ("cubesandbox-cold", "Cube cold", False, "cubesandbox"),
    ("microsandbox", "microsandbox", False, "microsandbox"),
    ("kata", "Kata", False, "kata"),
    ("runsc", "+gVisor", False, "docker"),
    ("runc", "Docker", False, "docker"),
    ("podman", "Podman", False, "podman"),
    ("bwrap", "bwrap", False, None),
    ("process", "Process", False, None),
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


def logo(c, name, x, y, size=18, theme="light"):
    if name:
        file = f"sandstorm-{theme}" if name == "sandstorm" else name
        data = base64.b64encode((LOGOS / f"{file}.png").read_bytes()).decode()
        c.out.append(f'<image x="{x}" y="{y}" width="{size}" height="{size}" href="data:image/png;base64,{data}"/>')


def hbars(c, x0, x1, y0, items, lo, hi, fmt, ticks=(), blank=None):
    """Log-scale horizontal bars between x0 and x1, one per row; `blank` says
    why a row has none."""
    import math
    t = c.t
    a, b = math.log10(lo), math.log10(hi)
    sx = lambda v: x0 + (x1 - x0) * (math.log10(max(v, lo)) - a) / (b - a)
    yb = y0 + ROW * len(items)
    c.out.append(f'<line x1="{x0:.1f}" x2="{x0:.1f}" y1="{y0}" y2="{yb}" stroke="{t["axis"]}"/>')
    for v in ticks:
        X = sx(v)
        c.out.append(f'<line x1="{X:.1f}" x2="{X:.1f}" y1="{y0}" y2="{yb}" stroke="{t["grid"]}"/>')
        c.text(X, yb + 18, fmt(v), anchor="middle")
    for i, (v, colour) in enumerate(items):
        y = y0 + 4 + i * ROW
        if v is None:
            if blank:
                c.text(x0 + 6, y + 15, blank)
            continue
        w = sx(v) - x0
        c.out.append(f'<rect x="{x0}" y="{y + 2}" width="{w:.1f}" height="16" rx="3" fill="{colour}"/>')
        c.text(sx(v) + 6, y + 15, fmt(v), size=12, colour=t["ink"])


MS = lambda v: f"{v / 1000:.1f} s" if v >= 1000 else f"{v:.0f} ms"


def compare(src, theme):
    """The figure at the top of the README, wide: four rankings of the 64 MiB
    app, each in its own order, since one order cannot rank them all. Its
    notes are the README's caption, not part of the image."""
    kvm = summaries(src)
    cloud, nodes = fleet_summaries(src)
    pvm, _ = fleet_summaries(src, "pvm")
    cl = clones(src)
    if not nodes or len(kvm) < 2 or "sandstorm-fc" not in cl:
        return None
    t = THEMES[theme]
    app = lambda d, k: d.get(k, {}).get("app64", (None, None))
    fork = {"sandstorm-fc-warm": "sandstorm-fc", "sandstorm-gvisor-warm": "sandstorm-gvisor", "cubesandbox": "cubesandbox", "microsandbox": "microsandbox"}
    forked = lambda k, f: f(cl[fork[k]]) if k in fork and fork[k] in cl else None
    mib = lambda v: f"{v:.0f} MiB"
    plots = [
        (0, "Start, KVM machine", lambda k: app(kvm, k)[0], (30, 5000), MS),
        (1, "Start, cloud VM", lambda k: app(cloud, k)[0] or app(pvm, k)[0], (30, 5000), MS),
        (2, "Memory per instance", lambda k: app(kvm, k)[1], (5, 500), mib),
        (3, "Clone into 20", lambda k: forked(k, lambda c: c[1][2]), (100, 5000), MS),
        (3, "Snapshot a running one", lambda k: forked(k, lambda c: c[0] or None), (10, 1000), MS),
    ]
    c = Chart(theme, "Sandstorm against the other sandboxes", "The 64 MiB app. Each column ranked on its own, best first; Sandstorm in blue. Log scales.",
              h=108 + ROW * len(ROWS) + 36, left=24, right=24, title_left=24, w=W, bottom=0)
    gap, label, tail = 18, 114, 52
    cw = (W - 48 - 3 * gap) / 4
    used = [c.top + 18] * 4
    for col, head, get, (lo, hi), fmt in plots:
        ranked = sorted(((get(r[0]), r) for r in ROWS if get(r[0])), key=lambda x: x[0])
        x, y0 = 24 + col * (cw + gap), used[col]
        c.text(x, y0 - 8, head, size=13, colour=t["ink"], weight=600)
        for i, (v, (_, name, _, mark)) in enumerate(ranked):
            logo(c, mark, x, y0 + 4 + i * ROW + 1, 18, theme)
            c.text(x + 23, y0 + 4 + i * ROW + 15, name, size=12, colour=t["ink"])
        hbars(c, x + label, x + cw - tail, y0, [(v, t["series"][0] if r[2] else t["muted"]) for v, r in ranked], lo, hi, fmt)
        used[col] = y0 + ROW * len(ranked) + 44
    return c.svg("Four rankings of a 64 MiB app by runtime: start on a KVM machine, start on a cloud VM, memory, and clone and snapshot time")


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
