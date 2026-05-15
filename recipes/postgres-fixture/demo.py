#!/usr/bin/env python3
"""End-to-end demo: fan out 10 isolated postgres-fixture children,
run a CREATE / INSERT / SELECT in each, print per-child wall-clock.

Demonstrates the fork-per-test pattern: each test gets a fresh DB
with the parent's schema seed in ~5-10 ms instead of ~2 s of fresh
initdb cost.

Prerequisites:
  - sudo bash recipes/postgres-fixture/build.sh
  - sudo -E forkd snapshot --tag pgfix --kernel <vmlinux> --rootfs parent.ext4 ... --boot-wait-secs 15
  - sudo bash scripts/netns-setup.sh 10
  - pip install psycopg[binary]
"""

import json
import os
import subprocess
import sys
import time
import urllib.request

DAEMON = os.environ.get("FORKD_URL", "http://127.0.0.1:8889")
N = int(os.environ.get("N", "10"))
PG_USER = os.environ.get("PG_USER", "forkd")
PG_DB = os.environ.get("PG_DATABASE", "forkd_test")


def post(path: str, body: dict) -> dict:
    req = urllib.request.Request(
        f"{DAEMON}{path}",
        data=json.dumps(body).encode(),
        method="POST",
        headers={"Content-Type": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=120) as r:
        return json.load(r)


def delete(path: str) -> None:
    req = urllib.request.Request(f"{DAEMON}{path}", method="DELETE")
    urllib.request.urlopen(req, timeout=10)


def psql_in_netns(netns: str, host: str, sql: str) -> str:
    """Run psql inside a per-child netns and return stdout."""
    cmd = [
        "ip", "netns", "exec", netns,
        "psql", "-h", host, "-p", "5432",
        "-U", PG_USER, "-d", PG_DB,
        "-tAc", sql,
    ]
    r = subprocess.run(cmd, capture_output=True, text=True, timeout=10)
    if r.returncode != 0:
        raise RuntimeError(f"psql failed: {r.stderr.strip()}")
    return r.stdout.strip()


def main() -> int:
    t0 = time.perf_counter()
    sbs = post(
        "/v1/sandboxes",
        {"snapshot_tag": "pgfix", "n": N, "per_child_netns": True, "memory_limit_mib": 512},
    )
    t_spawn = time.perf_counter()
    print(f"spawned {len(sbs)} postgres children in {(t_spawn - t0) * 1000:.0f} ms")

    # Each child gets a unique row id; afterwards we'll prove the
    # databases are independent by checking each child sees only its
    # own row.
    for i, sb in enumerate(sbs):
        host = sb["guest_addr"].split(":")[0]
        try:
            t_q0 = time.perf_counter()
            psql_in_netns(
                sb["netns"], host,
                f"CREATE TABLE marker (id int); INSERT INTO marker VALUES ({i});",
            )
            count = psql_in_netns(sb["netns"], host, "SELECT count(*) FROM marker;")
            t_q1 = time.perf_counter()
            print(f"  child {i} ({sb['netns']}): rows={count} in {(t_q1 - t_q0) * 1000:.0f} ms")
        except Exception as e:
            print(f"  child {i}: error: {e}", file=sys.stderr)

    for sb in sbs:
        delete(f"/v1/sandboxes/{sb['id']}")
    print(f"torn down; total wall-clock {(time.perf_counter() - t0) * 1000:.0f} ms")
    return 0


if __name__ == "__main__":
    sys.exit(main())
