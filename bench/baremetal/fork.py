#!/usr/bin/env python3
"""Snapshot a running sandbox and fork it, against a Sandstorm node.

    sudo python3 fork.py --name sandstorm-fc --master https://127.0.0.1:18081 \
        --pki /var/lib/sst-fc/pki --out fork.jsonl

The source is app64-warm on port 8000, answering. Then:

    snapshot   `branch` it 6 times; the first is dropped
    fork 1     a child from the last tag, to its first 200, 10 times after one
    fork n     n children in one call, until every one has answered, 3 rounds

Everything polled every 5 ms, as compare.py does. The same steps, the same
way, as CubeSandbox's snapshot and SDK clone were measured on bm
(bench/fleet/results/2026-10-29-cubesandbox).
"""

import argparse
import concurrent.futures as cf
import json
import ssl
import statistics
import time
import urllib.request
from pathlib import Path


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--name", required=True)
    ap.add_argument("--master", required=True)
    ap.add_argument("--pki", required=True)
    ap.add_argument("--template", default="app64-warm")
    ap.add_argument("--out", required=True)
    a = ap.parse_args()
    tok = (Path(a.pki) / "api-token").read_text().strip()
    ctx = ssl.create_default_context(cafile=str(Path(a.pki) / "ca.pem"))
    ctx.check_hostname = False

    def call(method, path, body=None):
        req = urllib.request.Request(
            a.master + path,
            data=json.dumps(body).encode() if body is not None else None,
            method=method,
            headers={"authorization": f"Bearer {tok}", "content-type": "application/json"},
        )
        with urllib.request.urlopen(req, context=ctx, timeout=180) as r:
            raw = r.read()
            return json.loads(raw) if raw else None

    def answered(sb, secs=120):
        req = urllib.request.Request(f"http://127.0.0.1:{sb['gateway_port']}/",
                                     headers={"x-sandstorm-sandbox-token": sb["token"]})
        end = time.time() + secs
        while time.time() < end:
            try:
                with urllib.request.urlopen(req, timeout=2) as r:
                    if r.status == 200:
                        return time.time()
            except Exception:
                time.sleep(0.005)
        return None

    def p50(v):
        return round(statistics.median(v), 1)

    parent = call("POST", "/v1/sandboxes", {"template": a.template, "port": 8000})
    assert answered(parent), "the source never answered"
    stamp = int(time.time())

    snap = []
    for i in range(6):
        t = time.time()
        call("POST", f"/v1/sandboxes/{parent['id']}/branch", {"tag": f"cmp-{stamp}-{i}"})
        snap.append((time.time() - t) * 1000)
        answered(parent, 30)
    tag = f"cmp-{stamp}-5"

    one = []
    for i in range(11):
        t = time.time()
        kid = call("POST", "/v1/sandboxes", {"snapshot_tag": tag, "n": 1})[0]
        done = answered(kid)
        one.append((done - t) * 1000)
        call("DELETE", f"/v1/sandboxes/{kid['id']}")
        time.sleep(0.5)

    batches = {}
    for n in (10, 20):
        walls = []
        for _ in range(3):
            t = time.time()
            kids = call("POST", "/v1/sandboxes", {"snapshot_tag": tag, "n": n})
            with cf.ThreadPoolExecutor(n) as ex:
                ends = list(ex.map(answered, kids))
            ok = [e for e in ends if e]
            walls.append((max(ok) - t) * 1000 if len(ok) == n else float("nan"))
            for k in kids:
                call("DELETE", f"/v1/sandboxes/{k['id']}")
            time.sleep(3)
        batches[n] = walls
    call("DELETE", f"/v1/sandboxes/{parent['id']}")

    row = {"runtime": a.name, "kind": "fork", "template": a.template,
           "snapshot_ms": p50(snap[1:]), "snapshot_all_ms": [round(x) for x in snap],
           "fork1_ms": p50(one[1:]),
           **{f"fork{n}_wall_ms": p50(w) for n, w in batches.items()},
           **{f"fork{n}_rounds_ms": [round(x) for x in w] for n, w in batches.items()}}
    open(a.out, "a").write(json.dumps(row) + "\n")
    print(json.dumps(row), flush=True)


if __name__ == "__main__":
    main()
