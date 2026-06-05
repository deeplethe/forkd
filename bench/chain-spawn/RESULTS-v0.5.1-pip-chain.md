# v0.5.1 pip-install chain bench

Real-package follow-up to [`RESULTS-v0.5.md`](./RESULTS-v0.5.md). Same host, same FC, same daemon — but with the v0.5.1 guest kernel (Linux 6.1.141) so `pip install` actually works inside the guest. The Phase 5 bench had to use stdlib-only Python source deltas because `pip install` hung on CRNG starvation (#218); this bench uses the real `pip install numpy → pandas → scikit-learn` chain that v0.5 was designed for.

## TL;DR

| | |
|---|---|
| **Per-link tax** | **~460 ms** (same as Phase 5 stdlib chain — model holds) |
| **Depth-3 vs compacted** | **1700 ms vs 78 ms** — 22× faster after `snapshot-compact` |
| **Strategy** | Chain to build, compact to ship |

## Setup

| | |
|---|---|
| Host | `yangdongxu-desktop` — Intel i7-12700, 32 GiB DDR4, ext4 |
| Kernel | host 6.14.0-36, **guest 6.1.141** (was 4.14.174 in v0.5.0) |
| FC | v1.12.0 + `mem_backend.shared` vendored patch |
| forkd | v0.5.1 (commit a1b32561) |
| Base (L0) | `demo-pyt` — `python:3.12-slim`, 512 MiB guest RAM |
| Iterations | 10 per head |
| Date | 2026-06-05 |

## Chain shape

```
demo-pyt (L0, base, python:3.12-slim)
   └── py-numpy   (L1: +numpy 2.0.2)            chain depth 1
        └── py-pandas  (L2: +pandas 2.2.3)       chain depth 2
             └── py-sklearn (L3: +scikit-learn 1.5.2)  chain depth 3
                  └── py-sklearn-flat (compact of py-sklearn)  depth 0
```

Built by feeding `forkd snapshot-diff --from <parent> --tag <child> --exec "pip install <pkg>=="<ver>""` for each layer. Wall-clock per build:

| layer | exec | build wall | diff bytes (FC's count) |
|---|---|---:|---:|
| py-numpy | `pip install numpy==2.0.2` | **27.2 s** | 222 MB |
| py-pandas | `pip install pandas==2.2.3` | **30.5 s** | 184 MB |
| py-sklearn | `pip install scikit-learn==1.5.2` | **60.6 s** | 380 MB |

Total chain build: ~2 minutes for the full numpy/pandas/sklearn stack.

## Spawn phase

`POST /v1/sandboxes` HTTP round-trip, 10 iters per head. Each iter kills any orphan FC + sleeps 0.5 s to give the tap device a chance to clear (forkd ships one tap per VMM).

| head | depth | p50 (ms) | p90 (ms) | max (ms) | min (ms) |
|---|---:|---:|---:|---:|---:|
| L0 (base `demo-pyt`) | 0 | **75** | 79 | 92 | 69 |
| L1 (`+numpy`) | 1 | **778** | 786 | 787 | 752 |
| L2 (`+pandas`) | 2 | **1 229** | 1 258 | 1 308 | 1 224 |
| L3 (`+sklearn`) | 3 | **1 700** | 1 703 | 1 706 | 1 687 |
| Flat-equiv (`py-sklearn-flat`) | 0 | **78** | 79 | 81 | 72 |

Per-link incremental tax (p50):

| from → to | Δ p50 (ms) |
|---|---:|
| L0 → L1 | **+703** |
| L1 → L2 | **+451** |
| L2 → L3 | **+471** |
| **L3 (depth 3) vs Flat-equivalent** | **+1 622** |

L0→L1 is slightly higher than the later increments because it includes the chain handler's per-spawn fixed cost (verify schema, build the resolver closure). L1→L2 and L2→L3 are pure SHA-256 of one more base-sized memory image — ~460 ms at the host CPU's ~1.1 GiB/s SHA-256 throughput. Same model the Phase 5 bench fit.

## L0 vs Flat-equivalent: 75 vs 78 ms

These are within noise. The two are different snapshots (one is the original `python:3.12-slim` base, the other was produced by `snapshot-compact py-sklearn → py-sklearn-flat`) but spawning either takes ~75 ms because both have `parent_tag = None` and the daemon takes the historical non-chain fast path.

This is the **headline operational guidance**:

> Build with `snapshot-diff` chains, ship with `snapshot-compact` to flatten.
>
> A chain stores its lineage compactly during agent iteration / experimentation (no need to re-pip-install when one upstream layer changes), but every spawn pays ~460 ms × depth. Once the chain stabilizes, one `snapshot-compact` collapses the per-link tax to zero forever.

## Disk

| | logical | du -sh |
|---|---:|---:|
| demo-pyt | 513 MiB | 513 M |
| py-numpy | 513 MiB | 513 M |
| py-pandas | 513 MiB | 513 M |
| py-sklearn | 513 MiB | 513 M |
| py-sklearn-flat | 513 MiB | 513 M |

Same story as Phase 5: FC's diff snapshots write a fixed-size `memory.bin` with zeros for unchanged pages rather than punching holes, so on ext4 every link weighs in at the full base size. The actual *changed* bytes per link are ~200–380 MiB per the `diff_physical_bytes` numbers FC reports during BRANCH, but those aren't visible to `du` without a reflink filesystem.

On btrfs / xfs with reflink, the `assemble_chain_memory` call in `crates/forkd-vmm/src/chain.rs` issues `ioctl(FICLONE)` for the base copy so blocks share with the parent — disk savings would be real there. Untested in this round; flagged for a v0.5.2 follow-up.

## What changed vs Phase 5

The two benches measure the same thing — `POST /v1/sandboxes` HTTP RTT for chains of varying depth on a 512 MiB base. Numbers are within noise of each other:

| | Phase 5 (stdlib delta) | v0.5.1 (real pip) | Δ |
|---|---:|---:|---:|
| L0 p50 | 59 ms | 75 ms | +27 % (cold-cache after a fresh boot) |
| L1 p50 | 751 ms | 778 ms | +4 % |
| L2 p50 | 1 222 ms | 1 229 ms | +1 % |
| L3 p50 | 1 668 ms | 1 700 ms | +2 % |
| Per-link tax | ~460 ms | ~460 ms | — |

The big behavioral difference is the **Flat-equivalent** row. Phase 5's "Flat" was a separately-built single-link snapshot (`chain-bench-flat`), so it still paid one chain hop (~746 ms p50). This bench's "Flat" is `py-sklearn-flat` produced by `snapshot-compact`, which actually sets `parent_tag = None` and restores via the original Phase 1 non-chain path — **78 ms p50**.

That's the v0.5 design rounding out: chains compose, compact flattens, both with predictable cost.

## Probe correctness on the chain

After the chain built, one POST /v1/sandboxes against `py-sklearn` → exec `python3 -c "import numpy, pandas, sklearn; from sklearn.linear_model import LinearRegression; ..."`:

```
numpy    2.0.2
pandas   2.2.3
sklearn  1.5.2
sklearn.LinearRegression fitted, coef=[1.0, 1.9999999999999993] intercept=3.0
```

100 % import success across all three layers, plus the fitted-model probe runs to completion. The vmstate-drift question — closed in Phase 5 for synthetic deltas — stays closed for real PyPI packages.

## Reproducing

```sh
# 1. Build the chain (one-time, ~2 min):
forkd snapshot-diff --from demo-pyt   --tag py-numpy   --exec "pip install numpy==2.0.2"
forkd snapshot-diff --from py-numpy   --tag py-pandas  --exec "pip install pandas==2.2.3"
forkd snapshot-diff --from py-pandas  --tag py-sklearn --exec "pip install scikit-learn==1.5.2"

# 2. Compact for prod (one-time, a few seconds):
forkd snapshot-compact --from py-sklearn --to py-sklearn-flat

# 3. Spawn from either (chain head ~1.7 s, flat ~78 ms):
forkd fork --tag py-sklearn      -n 1   # chain
forkd fork --tag py-sklearn-flat -n 1   # compacted

# 4. Bench harness used here:
scripts/dev/v05-e2e.sh   # asserts the chain semantics
# (a dedicated spawn-bench script lives at bench/chain-spawn/bench-chain-spawn.py
# for Phase 5; it accepts --base-tag and can be re-pointed at py-sklearn.)
```

## Operational takeaways

- **~1.7 s** is the price of a depth-3 chain on a 512 MiB base, dominated by per-link SHA-256.
- That tax is **deterministic and per-base-MiB** (~460 ms / 512 MiB ≈ 0.9 ms / MiB on this CPU). A 2 GiB chain head would be ~7 s at depth 3.
- The v0.6 mmap-once-then-incremental SHA verify (queued from the Phase 5 design) is the right path to cut this — it would amortize each parent's SHA over the lifetime of the daemon process rather than per-spawn.
- Until then: build with chain, ship with compact.
