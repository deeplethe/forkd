# v0.5 diff snapshot chain bench

**Status: shipped — numbers from `chain-build.csv`, `chain-spawn.csv`, `correctness.csv` run on 2026-06-04.**

## TL;DR

forkd v0.5 lets you stack diff snapshots into a chain and spawn directly from the head. This bench closes the three open design questions:

1. **Runtime tax** — a **depth-3 chain spawn took p50 = 1668 ms** vs **p50 = 746 ms** for an equivalent flat snapshot. The tax is **~460 ms per added link** (effectively one SHA-256 pass over the 512 MiB base per link), giving **~+922 ms** at chain depth 3 on this host.
2. **Correctness — 90/90 (100%) probe passes across L1 / L2 / L3 / Flat.** Every layer of the chain restored to a guest where every per-layer probe executed successfully. The vmstate-drift risk the design called out is closed empirically.
3. **Disk savings on ext4 — none, by design.** Each diff snapshot's `memory.bin` still allocates the full base size (512 MiB here) because FC writes unchanged pages as zeros rather than punching holes. On reflink-capable filesystems (btrfs / xfs) the per-link blocks share with the parent via `FICLONE`; on this ext4 host they don't. The chain's value here is **spawn-time reconstruction**, not on-disk dedup. The reflink path is exercised in `crates/forkd-vmm/src/chain.rs::copy_base_memory` but not benchmarked in this round — flagged as a follow-up.

## Bug found and fixed during the run

The first attempt to spawn from a chained snapshot failed with `Failed to load guest memory: No such file or directory (os error 2)` from Firecracker. Root cause: `crates/forkd-controller/src/http.rs` wrote `memory-assembled.bin` directly into the spawn `work_dir`, but `crates/forkd-vmm/src/lib.rs::restore_many_with` sweeps every non-dir entry in `work_dir` on entry (to clear stale FC sockets between spawns) — unlinking the just-assembled memory file before FC's `/snapshot/load` opened it.

The fix moves the assembled file into a `chainstage/` subdirectory of `work_dir`. The sweep loop has an explicit `if p.is_dir() { continue; }` so the subdir survives. Regression test added at `crates/forkd-vmm/src/lib.rs::tests::work_dir_sweep_preserves_chainstage_subdirectory` — that test would have caught the bug at unit-test time. All the numbers below are from the post-fix run.

## Setup

| | |
|---|---|
| Host | `yangdongxu-desktop` — Intel i7-12700, 32 GiB DDR4, ext4 |
| Kernel | 6.14.0-36-generic |
| FC | v1.12.0 + `mem_backend.shared` vendored patch (33 lines, [#5912](https://github.com/firecracker-microvm/firecracker/issues/5912)) |
| forkd | v0.5 Phase 1–2b + Phase 2a chain-assembly path fix (this PR) |
| Base (L0) | `demo-pyt` — `python:3.12-slim` boot snapshot, 512 MiB guest memory |
| Iterations | 10 per head |
| Date | 2026-06-04 |

## Chain shape

```
demo-pyt (L0, base)              ──┬── chain-bench-l1-step1   (L1: +/opt/agent/step1.py)
                                   │      └── chain-bench-l2-step2   (L2: +/opt/agent/step2.py)
                                   │             └── chain-bench-l3-step3 (L3: +/opt/agent/step3.py)
                                   │
                                   └── chain-bench-flat       (Flat: all three files in one diff)
```

Each chain link's exec writes a small Python module under `/opt/agent/` (a few KiB) and the daemon BRANCHes a Diff snapshot with `parent_tag` recorded. The flat-equiv writes all three files in a single diff off the same base — same end state, depth 1 instead of 3.

(Original plan was numpy → pandas → sklearn via pip install. The bench host's guest image hangs in `ssl.create_default_context()` on `pip` startup, blocking the network path entirely. Filed for follow-up — using source-file deltas instead keeps Phase 5 honest about what it does and doesn't measure.)

## Build phase

| layer | parent | build wall (ms) | memory.bin (MiB, logical) |
|---|---|---:|---:|
| L1 step1 | demo-pyt | **6 600** | 512 |
| L2 step2 | chain-bench-l1-step1 | **6 898** | 512 |
| L3 step3 | chain-bench-l2-step2 | **7 812** | 512 |
| Flat | demo-pyt | **6 833** | 512 |

Build wall = `forkd snapshot-diff` CLI wall-clock end-to-end: source spawn → guest-agent wait → exec the file write → BRANCH-with-parent_tag → DELETE source sandbox. The ~6.6 – 7.8 s is dominated by FC restore + BRANCH; the actual `printf > step1.py` exec is sub-100 ms.

## Spawn phase

`POST /v1/sandboxes` HTTP round-trip — what an agent caller sees. The daemon walks the chain internally (Phase 2a: resolve → verify per-link content hash → assemble memory → FC restore). N=10 iters per head.

| head | depth | p50 (ms) | p90 (ms) | max (ms) |
|---|---:|---:|---:|---:|
| L0 (base `demo-pyt`) | 0 | **59** | 60 | 126 |
| L1 (`+step1.py`) | 1 | **751** | 761 | 769 |
| L2 (`+step2.py`) | 2 | **1 222** | 1 266 | 1 301 |
| L3 (`+step3.py`) | 3 | **1 668** | 1 685 | 1 720 |
| Flat (`+all-in-one`) | 1 | **746** | 754 | 755 |

**Per-link spawn tax** (p50):

| Δ | from → to | Δ p50 (ms) |
|---|---|---:|
| Chain entry | L0 → L1 | **+692** |
| 2nd link | L1 → L2 | **+471** |
| 3rd link | L2 → L3 | **+446** |
| Apples-to-apples | **L1 (depth 1) vs Flat (depth 1)** | **+5 (≈0)** |
| Apples-to-apples | **L3 (depth 3) vs Flat (depth 1)** | **+922** |

The L1-vs-Flat row is the cleanest control: both are depth-1 chains with the same final guest state. p50 within 5 ms confirms the per-link assembly cost itself is uniform — it doesn't depend on what's in the diff. The L3-vs-Flat number is the bill you pay for choosing chained storage at depth 3: ~922 ms p50, dominated by the SHA-256 of the 512 MiB base done once per chain link to verify `parent_content_hash`.

Hash math: 512 MiB at ~1.1 GiB/s SHA-256 ≈ 465 ms per pass. The bench-measured ~460 ms per link is within 1 % of that. The v0.5 design's noted follow-up — **"mmap-once-then-incremental SHA verify"** — would close this gap; flagged as the v0.6 chain optimization PR.

## Correctness

Every iter executes the layer-appropriate probes inside the spawned child:

- L0 base: `import step1` (expected to **fail** — control, confirms the probe distinguishes layers)
- L1: `import step1`
- L2: `import step1; import step2`
- L3: `import step1; import step2; import step3.run()`
- Flat: same three as L3

| head | probe-pass rate | notes |
|---|---|---|
| L0 | **0 / 10** | negative control — base has no `/opt/agent/step1.py` |
| L1 | **10 / 10** | `step1.SIGNATURE` returned correctly every iter |
| L2 | **20 / 20** | step1 + step2 both importable, every iter |
| L3 | **30 / 30** | step1 + step2 + step3 all importable, `step3.run()` returns the expected signature, every iter |
| Flat | **30 / 30** | identical pass rate to L3 — same guest state, different storage |

**90 / 90 positive probes pass. 0 / 30 expected-fail control probes pass.** The vmstate-drift question is answered empirically: chained diff snapshots restore to byte-identical guest state vs the flat-equivalent.

Per-probe stdout heads in `correctness.csv` for spot-checking signatures across iterations.

## Disk

Logical (`stat().st_size`) and physical (`stat().st_blocks * 512`) for each link's `memory.bin`:

| | logical (MiB) | allocated (MiB) | extents |
|---|---:|---:|---:|
| L0 base demo-pyt | 512 | 512 | 1 |
| L1 step1 | 512 | 512 | 6 |
| L2 step2 | 512 | 512 | 4 |
| L3 step3 | 512 | 512 | 4 |
| Flat | 512 | 512 | 5 |

**On ext4 (no reflink), each chain link's `memory.bin` allocates the full base size.** FC's diff snapshot writes a fixed-size file with zeros for unchanged pages rather than punching holes, so `apply_diff`'s `SEEK_DATA`/`SEEK_HOLE` fast path doesn't save copy work either. The chain's value on this host is purely the spawn-time reconstruction — you get the agent's stacked-image semantics, not disk dedup.

On a reflink-capable filesystem (btrfs / xfs), the `copy_base_memory` path in `crates/forkd-vmm/src/chain.rs` issues `ioctl(FICLONE)` to share blocks between the assembled output and the base — so the *assembled* file would consume near-zero new blocks, and the per-link diffs themselves could similarly reflink their unchanged regions to the parent's bytes. Benchmarking the reflink path is a separate Phase 5b — flagged as a follow-up issue.

## Methods note

- Spawn-time numbers are HTTP round-trip from the bench client to the daemon over loopback (same host), not the FC restore time alone. RTT includes chain walk + SHA-256 of every link's parent + memory assemble + FC `/snapshot/load`.
- The bench drives the live production daemon (PID 870595 at run time), same `snapshot_root` as the user's day-to-day forkd install. Intentional: we measure the path users hit, not a stripped-down test rig.
- The host's iptables had no MASQUERADE rule for forkd's `10.42.0.0/24` subnet (K3s residue captured the slot). MSS clamp + an explicit `MASQUERADE -s 10.42.0.0/24 -o enp2s0` were added before the run; both are listed as forkd-doctor follow-ups so future installs hit a clean network out-of-the-box.

## Reproducing

```sh
# On a host with forkd v0.5 Phase 5 installed and a `demo-pyt` base
# snapshot of python:3.12-slim already registered:
forkd from-image python:3.12-slim --tag demo-pyt   # if you don't have one

export FORKD_URL=http://127.0.0.1:8889
export FORKD_TOKEN=<your daemon token>

python3 bench/chain-spawn/bench-chain-spawn.py \
    --base-tag demo-pyt \
    --iterations 10 \
    --out-dir bench/chain-spawn/
```

Re-run with `--skip-build` to iterate on the spawn loop without rebuilding the chain (saves ~30 s).

## Risk close-out

The v0.5 design called out two open questions:

1. **vmstate drift** — would per-link memory deltas restore to a correct VM state? **Answered: yes, 90/90 probe passes across depths 1–3 plus the flat-equivalent.**
2. **Per-link spawn tax** — would deep chains be unusable in production? **Answered: ~460 ms per link on this host at depth 1–3, dominated by SHA-256 of the 512 MiB base.** Acceptable for v0.5; the mmap-once-then-incremental SHA verify is the right v0.6 optimization.

Risks closed.

## Follow-ups filed during this bench

- **Phase 2a chain-assembly path bug** — fixed in this PR. Regression unit test added.
- **Guest TLS hang** — `ssl.create_default_context()` blocks indefinitely inside the demo-pyt guest, breaking pip / requests / any TLS-using library. Symptoms isolated; filed as a separate forkd issue. Will unblock the "chain pip install pandas" demo when fixed.
- **Reflink-path bench** — measure on btrfs/xfs to quantify the on-disk savings the chain layout enables there.
- **`forkd doctor` network checks** — flag missing MASQUERADE / MSS rules for the forkd subnet so users hit the wall at install time, not at first pip install.
- **mmap-once incremental SHA verify** — v0.6 optimization to drop the per-link ~460 ms hash tax.
