#!/usr/bin/env python3
"""
dirtier.py — controlled memory-dirtying workload for diff-snapshot bench.

Allocates `--dirty-mib N` MiB and writes one non-zero byte per 4 KiB
page, then sleeps. This dirties exactly N MiB of guest memory, which
shows up as `diff_physical_bytes` in the daemon's BRANCH response.

The orchestrator (sweep-agent.sh) execs this script via the daemon's
exec endpoint, waits for the "READY_TO_BRANCH" marker on stdout, then
triggers BRANCH. The script holds its buffer alive across the BRANCH
window so the dirty bitmap is not freed before firecracker reads it.

No third-party deps (runs on any python:3.12-slim-class rootfs).

Stdout schema (one JSON event per line, plus the marker):

    {"event":"start", "dirty_mib": int, "t_ms": int}
    {"event":"dirtied", "dirty_mib": int, "wall_ms": int, "t_ms": int}
    READY_TO_BRANCH
    {"event":"stop", "t_ms": int}
"""
import argparse
import json
import sys
import time


def emit(obj):
    sys.stdout.write(json.dumps(obj, separators=(",", ":")))
    sys.stdout.write("\n")
    sys.stdout.flush()


def now_ms():
    return int(time.time() * 1000)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--dirty-mib", type=int, required=True,
                    help="bytes to allocate AND touch (one byte per 4 KiB page)")
    ap.add_argument("--hold-s", type=float, default=120,
                    help="seconds to keep the dirty buffer alive after marker")
    args = ap.parse_args()

    emit({"event": "start", "dirty_mib": args.dirty_mib, "t_ms": now_ms()})

    t0 = now_ms()
    size = args.dirty_mib * 1024 * 1024
    # bytearray is contiguous in CPython — we get a single ~N MiB
    # allocation backed by anonymous pages. Touching one byte per
    # 4 KiB page is what makes them count as dirty in the KVM bitmap.
    buf = bytearray(size)
    # Note: stride must be exactly the kernel page size. If the rootfs
    # boots with hugepages this would need adjustment, but forkd's
    # default kernel uses 4 KiB.
    PAGE = 4096
    n_pages = size // PAGE
    for i in range(n_pages):
        # Non-zero value so the dirty bit is unambiguously set (a
        # zero-to-zero write might be skipped on some kernels;
        # conservative to write something the page didn't already have).
        buf[i * PAGE] = (i & 0xff) | 0x80
    emit({"event": "dirtied",
          "dirty_mib": args.dirty_mib,
          "wall_ms": now_ms() - t0,
          "t_ms": now_ms()})

    # Magic marker the orchestrator polls for. Must be on its own line
    # and flushed before sleep so the orchestrator can react promptly.
    sys.stdout.write("READY_TO_BRANCH\n")
    sys.stdout.flush()

    # Hold the buffer alive across the BRANCH window. The sleep is
    # blocking; the buffer stays in scope so its pages remain dirty.
    # (Garbage-collecting it before firecracker reads the bitmap would
    # leave the diff smaller than the user asked for.)
    time.sleep(args.hold_s)
    # Force a reference so the optimizer doesn't dead-code the buffer.
    _ = buf[0]
    emit({"event": "stop", "t_ms": now_ms()})


if __name__ == "__main__":
    main()
