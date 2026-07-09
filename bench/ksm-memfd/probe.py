#!/usr/bin/env python3
"""
Probe whether KSM merges MADV_MERGEABLE memfd MAP_SHARED mappings.

This is intentionally small and direct: it maps two same-sized regions,
fills both with identical page contents, marks the VMAs mergeable, then
waits for ksmd full scans and reports /sys/kernel/mm/ksm counters.

Run on a Linux host where the caller can write /sys/kernel/mm/ksm:

    sudo python3 bench/ksm-memfd/probe.py
"""

import argparse
import ctypes
import multiprocessing as mp
import os
import signal
import time

libc = ctypes.CDLL(None, use_errno=True)

MADV_MERGEABLE = 12
MAP_PRIVATE = 0x02
MAP_ANONYMOUS = 0x20
MAP_SHARED = 0x01
PROT_READ = 0x1
PROT_WRITE = 0x2
MAP_FAILED = ctypes.c_void_p(-1).value

PAGE = os.sysconf("SC_PAGE_SIZE")
PATTERN = b"forkd-ksm-page".ljust(PAGE, b"x")
KSM = "/sys/kernel/mm/ksm"

libc.mmap.restype = ctypes.c_void_p
libc.mmap.argtypes = [
    ctypes.c_void_p,
    ctypes.c_size_t,
    ctypes.c_int,
    ctypes.c_int,
    ctypes.c_int,
    ctypes.c_long,
]
libc.madvise.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_int]


def read_ksm():
    out = {}
    for name in [
        "pages_shared",
        "pages_sharing",
        "pages_unshared",
        "pages_volatile",
        "full_scans",
    ]:
        with open(f"{KSM}/{name}", "r", encoding="utf-8") as f:
            out[name] = int(f.read().strip())
    return out


def write_ksm(name, value):
    with open(f"{KSM}/{name}", "w", encoding="utf-8") as f:
        f.write(str(value))


def configure_ksm(pages):
    saved = {}
    for name in ["run", "pages_to_scan", "sleep_millisecs"]:
        with open(f"{KSM}/{name}", "r", encoding="utf-8") as f:
            saved[name] = f.read().strip()
    write_ksm("run", 2)
    wait_for(lambda counters: counters["pages_sharing"] == 0, 10)
    write_ksm("run", 1)
    write_ksm("pages_to_scan", max(pages * 4, 10000))
    write_ksm("sleep_millisecs", 20)
    return saved


def restore_ksm(saved):
    for name, value in saved.items():
        try:
            write_ksm(name, value)
        except OSError:
            pass


def mmap_region(size, flags, fd=-1):
    ptr = libc.mmap(None, size, PROT_READ | PROT_WRITE, flags, fd, 0)
    if ptr == MAP_FAILED:
        err = ctypes.get_errno()
        raise OSError(err, os.strerror(err))
    rc = libc.madvise(ctypes.c_void_p(ptr), size, MADV_MERGEABLE)
    if rc != 0:
        err = ctypes.get_errno()
        raise OSError(err, f"madvise MADV_MERGEABLE: {os.strerror(err)}")
    return ptr


def fill(ptr, pages):
    for i in range(pages):
        ctypes.memmove(ptr + i * PAGE, PATTERN, PAGE)


def child(kind, pages, ready):
    size = pages * PAGE
    if kind == "anonymous-private":
        ptrs = [
            mmap_region(size, MAP_PRIVATE | MAP_ANONYMOUS),
            mmap_region(size, MAP_PRIVATE | MAP_ANONYMOUS),
        ]
    elif kind == "memfd-map-shared-two-fds":
        fds = [
            os.memfd_create("forkd-ksm-a", os.MFD_CLOEXEC),
            os.memfd_create("forkd-ksm-b", os.MFD_CLOEXEC),
        ]
        for fd in fds:
            os.ftruncate(fd, size)
        ptrs = [mmap_region(size, MAP_SHARED, fd) for fd in fds]
    else:
        raise ValueError(kind)

    for ptr in ptrs:
        fill(ptr, pages)

    ready.send(os.getpid())
    signal.pause()


def wait_for(predicate, timeout):
    deadline = time.time() + timeout
    last = None
    while time.time() < deadline:
        last = read_ksm()
        if predicate(last):
            return last
        time.sleep(0.2)
    return last or read_ksm()


def run_case(kind, pages, timeout):
    write_ksm("run", 2)
    wait_for(lambda counters: counters["pages_sharing"] == 0, timeout)
    write_ksm("run", 1)
    before = read_ksm()
    parent, child_conn = mp.Pipe(duplex=False)
    proc = mp.Process(target=child, args=(kind, pages, child_conn))
    proc.start()
    parent.recv()
    if not proc.is_alive():
        raise RuntimeError(f"{kind} child exited before KSM scan")
    after = wait_for(
        lambda counters: counters["full_scans"] >= before["full_scans"] + 2
        and counters["pages_sharing"] > before["pages_sharing"],
        timeout,
    )
    proc.terminate()
    proc.join(timeout=5)
    wait_for(
        lambda counters: counters["pages_sharing"] <= before["pages_sharing"],
        timeout,
    )
    return before, after


def delta(before, after):
    return {k: after[k] - before[k] for k in before}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--pages", type=int, default=4096)
    parser.add_argument("--timeout", type=float, default=20.0)
    args = parser.parse_args()

    if not os.path.exists(f"{KSM}/run"):
        raise SystemExit("KSM sysfs is not available")

    saved = configure_ksm(args.pages)
    try:
        for kind in ["anonymous-private", "memfd-map-shared-two-fds"]:
            before, after = run_case(kind, args.pages, args.timeout)
            print(kind)
            print(f"  before={before}")
            print(f"  after ={after}")
            print(f"  delta ={delta(before, after)}")
    finally:
        restore_ksm(saved)


if __name__ == "__main__":
    main()
