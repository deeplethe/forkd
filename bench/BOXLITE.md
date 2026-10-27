# BoxLite bench methodology

## Result

BoxLite N=100 spawn measured at **113,209 ms** on the same dev box
sandstorm was measured on (Ubuntu 24.04 / Linux 6.14 / 20 vCPU / 30 GiB /
KVM). 0 of 100 sandboxes succeeded the workload (`import numpy`)
because `python:3.12-slim` doesn't ship numpy; this matches the
condition Docker, gVisor, and OpenSandbox were measured under, so
the wall-clock figure is comparable across container-shaped runners.

## Setup

```bash
python3.13 -m pip install --user boxlite==0.9.3
```

(BoxLite ships a manylinux wheel — no separate `boxlite-cli` /
`cargo install` step is needed for the Python SDK path. The
optional Rust CLI `boxlite-cli` requires `protoc` and a Rust
toolchain; we don't depend on it for benchmarking.)

## Workload

`bench/boxlite-bench.py` (also wired into [`compare-all.py`](./compare-all.py))
issues N concurrent `boxlite.SimpleBox(image="python:3.12-slim")`
contexts via `asyncio.gather`, runs `python -c "import numpy;
print(numpy.zeros(5).tolist())"` inside each, and tears down the
Box on context exit.

```python
import asyncio, boxlite

async def one(_i):
    async with boxlite.SimpleBox(image="python:3.12-slim") as box:
        r = await box.exec("python", "-c",
            "import numpy; print(numpy.zeros(5).tolist())")
        return getattr(r, "exit_code", 0) == 0

results = asyncio.run(asyncio.gather(*(one(i) for i in range(100))))
```

## Scaling profile observed

| N | total wall-clock | per-box mean |
|---:|---:|---:|
| 3 | 5,581 ms | 1,860 ms |
| 10 | 7,339 ms | 734 ms |
| 50 | 23,355 ms | 467 ms |
| 100 | 113,209 ms | 1,132 ms |

Per-box mean improves from N=3 to N=50 as parallelism amortises
overhead, then degrades at N=100 as the host saturates on disk I/O
(BoxLite reports `console.log` errors mentioning "Slow disk I/O
during rootfs setup" and "Guest agent failed to start" for some
boxes at N=100).

## Notes

BoxLite's positioning ("the SQLite of sandbox" — embeddable, no
daemon, cross-platform via Hypervisor.framework on macOS) is
optimised for **one long-lived Box per workload** rather than
N concurrent fresh Boxes. Cold-spawning 100 microVMs concurrently
on a single 30-GiB host is not the workload BoxLite was designed
for; the measurement is included here for comparability with the
other runners on the same task, not as a critique of the design.
