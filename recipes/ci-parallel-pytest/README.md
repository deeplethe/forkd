# `ci-parallel-pytest`

**Run your pytest suite across N forkd microVMs in parallel,
without paying per-worker container cold-start or dependency
import cost.**

A typical Python CI job re-imports numpy / pandas / scikit-learn on
every fresh worker container — ~1-2 s of pure overhead before the
first test runs. With forkd, those imports live in the warmed
parent's snapshot; every fork inherits them via `mmap MAP_PRIVATE`
copy-on-write. Per-worker fixed cost drops to ~50-100 ms.

## Architecture

```
                 ┌──────────────────────────────────────┐
                 │  parent snapshot `ci-pytest`         │
                 │  python:3.12-slim                    │
                 │  + pytest 8.3 numpy 2.0 pandas 2.2   │
                 │  + scikit-learn 1.5                  │
                 │  + your /opt/test_project            │
                 │  (heavy imports already paid)        │
                 └────────────────┬─────────────────────┘
                                  │  mmap MAP_PRIVATE (CoW)
            ┌─────────────────────┼─────────────────────┐
            │                     │                     │
       ┌────▼───────┐       ┌─────▼──────┐       ┌──────▼─────┐
       │ worker 1   │       │ worker 2   │       │ worker N   │
       │ pytest     │       │ pytest     │       │ pytest     │
       │ slice 1/N  │  ...  │ slice 2/N  │  ...  │ slice N/N  │
       └────────────┘       └────────────┘       └────────────┘
                            run in parallel
```

## What ships in this recipe

| File | What it does |
|---|---|
| [`build.sh`](./build.sh) | Builds a forkd parent rootfs: `python:3.12-slim` + pinned pytest/numpy/pandas/sklearn, the demo test project copied to `/opt/test_project`, and a pre-warm step that imports the heavy deps so they're in the snapshot's page cache |
| [`test_project/`](./test_project/) | A representative pytest project — ~30 tests across 5 files (arithmetic, numpy, pandas, sklearn, text). Replace with your own |
| [`demo.py`](./demo.py) | Fan-out driver: slices test files across N forkd workers, runs each slice in a child sandbox, reports per-worker spawn/exec timing + total wall-clock + sequential-baseline comparison |

## When to use this

- **CI pipelines with 100s of pytest tests** that re-import heavy
  ML libs every run. The savings compound: every PR run, every
  retry, every nightly.
- **PR-preview environments** where each PR needs its own clean
  pytest run with fresh side-effects (DB, filesystem, env). forkd's
  per-child KVM isolation means workers truly don't see each other.
- **Sharded fuzz / property testing**: split a 10 000-iteration
  Hypothesis run across N microVMs without setup tax.

## When NOT to use this

- Your test suite is < 30 tests and finishes in < 2 s sequentially —
  parallelism overhead exceeds the gain.
- You don't actually need per-worker isolation (e.g. pure-function
  unit tests with no shared state) — `pytest -n <N>` (pytest-xdist)
  in a single container is simpler.
- You can't run forkd on your CI host (managed CI like default
  GitHub Actions, no KVM). For self-hosted runners with bare-metal
  Linux + KVM this works great.

## Quickstart

```bash
# 1. Build the parent (one-time, ~5 min — pip install pandas + sklearn
#    dominates the time)
sudo bash recipes/ci-parallel-pytest/build.sh

# 2. Snapshot the warmed parent (one-time, ~10 s)
sudo forkd snapshot --tag ci-pytest \
    --kernel /var/lib/forkd/kernels/vmlinux \
    --rootfs recipes/ci-parallel-pytest/parent.ext4 \
    --tap forkd-tap0

# 3. Fan out — 4 workers in parallel
FORKD_TOKEN=$(sudo cat /tmp/bench-pause/token) \
    python3 recipes/ci-parallel-pytest/demo.py --workers 4 \
                                               --sequential-baseline
```

Expected output:

```
Plan: 4 worker(s) × pytest slice off `ci-pytest`.
  worker 0: 2 file(s) — test_arithmetic.py, test_text_processing.py
  worker 1: 1 file(s) — test_numpy_ops.py
  worker 2: 1 file(s) — test_pandas_etl.py
  worker 3: 1 file(s) — test_sklearn_models.py

=== fan-out: 4 workers in parallel ===
  [0] PASS  spawn=  76ms  exec= 612ms  files=test_arithmetic.py,test_text_processing.py
  [1] PASS  spawn=  79ms  exec=1184ms  files=test_numpy_ops.py
  [2] PASS  spawn=  78ms  exec= 891ms  files=test_pandas_etl.py
  [3] PASS  spawn=  74ms  exec=1502ms  files=test_sklearn_models.py

fan-out wall-clock:  1581 ms   (spawn p50=77 ms, slowest worker exec=1502 ms)

=== sequential baseline: one child runs the whole suite ===
  [0] PASS  spawn= 75ms  exec=2487ms
sequential wall-clock: 2562 ms   (parallel speedup vs slowest worker: 1.66×)
```

Numbers depend on host CPU + storage. The ones above are from an
i7-12700 / ext4 host. Replace your tests, re-measure.

## GitHub Actions integration

Drop this into your workflow on a self-hosted runner that has forkd
+ a `ci-pytest` snapshot pre-built:

```yaml
jobs:
  test:
    runs-on: [self-hosted, linux, x64, forkd]
    steps:
      - uses: actions/checkout@v4
      - name: Refresh the parent snapshot
        run: |
          sudo cp -r ./tests /opt/test_project/tests   # mount your tests into the snap dir
          # or rebuild the parent if your deps changed:
          # sudo bash recipes/ci-parallel-pytest/build.sh
      - name: Fan out
        env:
          FORKD_TOKEN: ${{ secrets.FORKD_TOKEN }}
        run: |
          python3 recipes/ci-parallel-pytest/demo.py \
              --workers 8 \
              --snapshot-tag ci-pytest
```

For a hosted-runner setup, the equivalent is one forkd daemon on
your CI infrastructure, exposed over a port the runner can reach.

## How it compares

| Approach | Per-worker fixed cost | Wall-clock 4 workers, this suite |
|---|---|---|
| `pytest` sequential, fresh container | ~2 s (container cold-start) + ~1.5 s (imports) | one container, ~4-5 s |
| `pytest-xdist -n 4` in one container | ~3.5 s container cold + ~1.5 s import (paid once, shared) | ~3 s |
| `docker run` × 4 fresh containers | ~3.5 s × 4 = 14 s (parallel: ~5 s) | ~5-7 s |
| **forkd fan-out (this recipe)** | **~80 ms spawn + 0 ms import** | **~1.6 s** |

The break-even point is roughly: if your sequential test slice is
slower than your container cold-start, container parallelism is
fine. If your slice is **comparable to or shorter than** the cold-
start tax, forkd wins.

## Caveats

- **`pip install` inside snapshots requires v0.5.1+** — the guest
  kernel rebuild that landed in #226 closed #218 (CRNG starvation
  blocked OpenSSL → pip hung). Confirm your kernel:
  `forkd snapshot-info ci-pytest`
- **Per-worker netns is on by default** — children get their own
  `lo`, no cross-talk. If your tests need to hit a shared DB, use
  `--per-child-netns=false` or put the DB on the host tap.
- **Worker count vs vCPU**: forkd's per-vCPU policy is "share the
  host's cores". On a 20-core host, 8 workers is comfortable; 50
  is over-subscribed.
