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

Output from the dev box (Intel i7-12700, ext4, 2026-06-06):

```
Plan: 4 worker(s) × pytest slice off `ci-pytest`.
  worker 0: 2 file(s) — test_arithmetic.py, test_text_processing.py
  worker 1: 1 file(s) — test_numpy_ops.py
  worker 2: 1 file(s) — test_pandas_etl.py
  worker 3: 1 file(s) — test_sklearn_models.py

=== fan-out: 4 workers in parallel ===
  batch spawn (4 children): 81 ms
  [0] PASS  exec= 232 ms  files=test_arithmetic.py,test_text_processing.py
  [1] PASS  exec= 304 ms  files=test_numpy_ops.py
  [2] PASS  exec= 546 ms  files=test_pandas_etl.py
  [3] PASS  exec=1458 ms  files=test_sklearn_models.py

fan-out wall-clock:  1601 ms   (batch spawn=81 ms = ~20 ms/worker,
                                slowest worker exec=1458 ms)

=== sequential baseline: one child runs the whole suite ===
  [0] PASS  spawn=61 ms  exec=1507 ms
sequential wall-clock: 1625 ms   (fan-out speedup: 1.01×)
```

The 1.01× fan-out-vs-sequential figure is honest: this demo suite
only has ~30 tests and is dominated by one sklearn slice (1458 ms).
Fan-out shines when **your suite has many slow slices of comparable
size** — e.g. 8 sklearn-heavy slices each taking ~1.5 s would fan
out to ~1.5 s wall, vs ~12 s sequentially.

**The number that matters across suite shapes is the batch spawn
cost: 81 ms for 4 children — ~20 ms per worker.** That's the
forkd-vs-container comparison: ~20 ms to start a forkd worker vs
~2-3 s to start a fresh container.

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

| Approach | Per-worker fixed cost | Notes |
|---|---|---|
| `pytest` sequential, fresh container | ~2 s container cold + ~1.5 s `import numpy/pandas/sklearn` | Each PR run / retry / nightly re-pays both |
| `pytest-xdist -n 4` in one container | ~3.5 s container cold + ~1.5 s imports (shared across workers) | Single shared kernel; one test crash takes the host down |
| `docker run` × 4 fresh containers | ~3.5 s × 4 cold-starts, parallelized | Per-container isolation, but slow to spawn |
| **forkd fan-out (this recipe)** | **~20 ms batch spawn + 0 ms imports** | Per-child KVM isolation, warmed Python deps inherited via mmap CoW |

The break-even point is roughly: if your sequential test slice is
slower than your container cold-start (~3 s), container
parallelism is fine. If your slice is **comparable to or shorter
than** the ~3 s container tax, forkd wins outright. ML / data
science suites where you re-pay sklearn / torch import on every
worker fall squarely in the forkd-wins zone.

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
