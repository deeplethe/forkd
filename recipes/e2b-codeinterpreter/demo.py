#!/usr/bin/env python3
"""End-to-end demo: fan out 10 sandstorm children from the E2B code-interpreter
parent, run a small pandas snippet in each, print the wall-clock and result.

Prerequisites:
  - sudo bash recipes/e2b-codeinterpreter/build.sh
  - sudo sandstorm snapshot --tag ci --kernel <vmlinux> --rootfs parent.ext4 ...
  - sudo bash scripts/netns-setup.sh 10
"""

import asyncio
import os
import sys
import time
import urllib.request
import json

DAEMON = os.environ.get("SANDSTORM_URL", "http://127.0.0.1:8889")
N = int(os.environ.get("N", "10"))


def post(path: str, body: dict) -> dict:
    req = urllib.request.Request(
        f"{DAEMON}{path}",
        data=json.dumps(body).encode(),
        method="POST",
        headers={"Content-Type": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=120) as r:
        return json.load(r)


def main() -> int:
    t0 = time.perf_counter()
    sbs = post(
        "/v1/sandboxes",
        {"snapshot_tag": "ci", "n": N, "per_child_netns": True, "memory_limit_mib": 256},
    )
    t_spawn = time.perf_counter()
    print(f"spawned {len(sbs)} sandboxes in {(t_spawn - t0) * 1000:.0f} ms")

    for sb in sbs[:3]:
        r = post(
            f"/v1/sandboxes/{sb['id']}/eval",
            {"code": "pandas.DataFrame({'a': [1,2,3]}).sum().to_dict()"},
        )
        print(f"  {sb['id']}: {r.get('result', r.get('error'))}")

    for sb in sbs:
        urllib.request.urlopen(
            urllib.request.Request(f"{DAEMON}/v1/sandboxes/{sb['id']}", method="DELETE"),
            timeout=10,
        )
    print(f"torn down in total {(time.perf_counter() - t0) * 1000:.0f} ms")
    return 0


if __name__ == "__main__":
    sys.exit(main())
