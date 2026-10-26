#!/usr/bin/env python3
"""A pretend fleet for working on the console.

    console-sim.py http://127.0.0.1:8898 <api-token> [nodes] [seconds]

Needs a master started with --insecure-transport, which takes the node id
from the poll body. Each simulated node does what a real one does with the
master's commands -- creates, tier moves, destroys -- and reports what it
holds, its free memory and pressure, the occasional sandbox that will not
start, and wakes from traffic at its gateway. One node drops out for a
minute now and then, so the console has something to notice.

Nothing here touches a real machine. Stdlib only.
"""
import json
import math
import random
import sys
import threading
import time
import urllib.error
import urllib.request

BASE, TOKEN = sys.argv[1].rstrip("/"), sys.argv[2]
NODES = int(sys.argv[3]) if len(sys.argv) > 3 else 6
SECS = int(sys.argv[4]) if len(sys.argv) > 4 else 6 * 3600
H = {"authorization": "Bearer " + TOKEN, "content-type": "application/json"}
START = time.time()


def call(method, path, body=None, timeout=10):
    req = urllib.request.Request(BASE + path, method=method, headers=H,
                                 data=json.dumps(body).encode() if body is not None else None)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            raw = r.read()
            return r.status, (json.loads(raw) if raw else None)
    except urllib.error.HTTPError as e:
        return e.code, None
    except (urllib.error.URLError, OSError):
        return 0, None


class Node(threading.Thread):
    def __init__(self, i):
        super().__init__(daemon=True)
        self.id = f"edge-{i + 1}"
        self.i = i
        self.held = {}          # id -> tier
        self.failing = {}       # id -> attempts
        self.rng = random.Random(i)
        self.mem = 1900 + 512 * (i % 3)

    def pressure(self):
        hot = sum(1 for t in self.held.values() if t == "hot")
        frozen = sum(1 for t in self.held.values() if t == "frozen")
        used = 380 + 15 * hot + 5 * frozen + 0.4 * len(self.held) + self.rng.uniform(-25, 25)
        free = max(40, int(self.mem - used))
        psi = round(max(0.0, (650 - free) / 45) + self.rng.uniform(0, 0.4), 1)
        return free, psi

    def run(self):
        while time.time() - START < SECS:
            el = time.time() - START
            # One node in six goes quiet for a minute every eight.
            if self.i == 3 and int(el) % 480 > 420:
                time.sleep(2)
                continue
            traffic = []
            cold = [s for s, t in self.held.items() if t in ("frozen", "snapshot")]
            for s in self.rng.sample(cold, min(len(cold), self.rng.choice([0, 0, 0, 1, 2]))):
                self.held[s] = "hot"     # the gateway wakes it before the master hears
                traffic.append(s)
            free, psi = self.pressure()
            body = {
                "node_id": self.id,
                "observed": [{"id": s, "tier": t, "serving": t != "detached",
                              "ip": "10.86.0.2", "net_index": None}
                             for s, t in self.held.items()],
                "free_memory_mib": free,
                "pressure_some_avg10": psi,
                "hold_ms": 0,
                "failures": [{"id": s, "error": err, "attempts": n}
                             for s, (n, err) in self.failing.items()],
                "traffic": traffic,
                "features": ["reclaim-frozen", "swap:zswap"] + (["snapshot-compression"] if self.i % 2 else []),
            }
            st, resp = call("POST", "/v1/node/poll", body)
            for c in (resp or {}).get("commands", []):
                op, sid = c.get("op"), c.get("id")
                if op == "create":
                    if sid in self.failing:
                        n, err = self.failing[sid]
                        self.failing[sid] = (n + 1, err)
                    elif self.rng.random() < 0.012:
                        self.failing[sid] = (1, self.rng.choice([
                            "runsc create: exited with status 128: cannot create gofer process",
                            "node held it back: 471 MiB free, below its own 512 MiB floor",
                            "setting up the network: ip netns add: file exists"]))
                    else:
                        self.held[sid] = "hot"
                elif op == "set_tier" and sid in self.held:
                    self.held[sid] = c["tier"]
                elif op == "destroy":
                    self.held.pop(sid, None)
                    self.failing.pop(sid, None)
            time.sleep(1.5)


def client():
    rng = random.Random(42)
    mine = []
    while time.time() - START < SECS:
        el = time.time() - START
        # A slow tide with bursts on top: creates rise and fall over ten
        # minutes, and every few minutes a batch job asks for a lot at once.
        rate = 2.5 + 2.5 * math.sin(el / 95) + (14 if int(el) % 300 < 8 else 0)
        for _ in range(max(0, int(rng.gauss(rate, 1)))):
            st, b = call("POST", "/v1/sandboxes", {"template": rng.choice(["python3.12", "python3.12", "node22"])})
            if st == 201 and b:
                mine.append(b["id"])
        # Deletes trail creates so the fleet breathes instead of only filling.
        keep = 900 + 300 * math.sin(el / 400)
        drop = rng.randint(0, 3) + (rng.randint(2, 6) if len(mine) > keep else 0)
        for _ in range(min(len(mine), drop)):
            call("DELETE", "/v1/sandboxes/" + mine.pop(rng.randrange(len(mine))))
        # Some callers come back to a sandbox they already have.
        for sid in rng.sample(mine, min(len(mine), 3)):
            call("POST", f"/v1/sandboxes/{sid}/touch")
        time.sleep(1)


def main():
    for i in range(NODES):
        nid = f"edge-{i + 1}"
        _, tok = call("POST", "/v1/node-tokens", {"pool": "default"})
        call("POST", "/v1/node/register", {"token": tok["token"], "node_id": nid,
                                           "fingerprint": "fp-epyc-7k62", "max_sandboxes": 0})
    for name, args in [("python3.12", ["/usr/bin/python3", "/app/server.py"]),
                       ("node22", ["/usr/bin/node", "/app/server.js"])]:
        call("POST", "/v1/templates", {"name": name, "rootfs": "rootfs",
                                       "spec": {"args": args, "memory_max_mib": 256}})
    for i in range(NODES):
        Node(i).start()
    client()


if __name__ == "__main__":
    main()
