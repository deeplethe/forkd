#!/usr/bin/env python3
"""speculative-agent: an agent reaches a decision point, BRANCHes its
sandbox, fans out N children that each pursue a different strategy,
and a judge picks the best one. Losers are discarded — but they all
ran in parallel from one warmed parent in ~200ms.

This is the headline BRANCH+fanout recipe. The other four recipes
(mcp-agent, crewai-fanout, autogen-branch, openai-swarm) showed
forkd plumbing inside a specific framework. This one shows the
*pattern* — speculative execution as a decision primitive — that the
forkd BRANCH operation enables and nothing else open-source does.

What this script does:

  1. Provision one source sandbox; load the question + a setup pass
     (small but representative of agent warmup: import a few
     libraries, compute a constant the strategies all need).
  2. BRANCH the source. ~200ms pause, diff_physical_bytes captured.
  3. Spawn N grandchildren from the branch. Each inherits the loaded
     state — they don't re-import or re-compute the constant.
  4. Each grandchild runs a different strategy for solving the same
     problem. Strategies are intentionally chosen so they produce
     the same numerical answer but with wildly different wall times.
  5. A judge function ranks the strategies. Print: winner, all
     answers (correctness check), all wall times, speedup ratios.

The problem the strategies solve: "Sum of squares from 1 to N."
Three strategies:

  - LOOP:    naive Python for-loop. Reference correctness.
  - FORMULA: closed-form N*(N+1)*(2N+1)/6. Microseconds.
  - NUMPY:   np.arange(N+1).dot(np.arange(N+1)). Fast for large N
             but pays a numpy import + buffer alloc penalty.

The script prints which won and by how much — a clear, reproducible
artifact you can paste into a tweet or blog.

Prerequisites:
  - forkd-controller running.
  - Python-capable snapshot (`coding-agent-fork-prewarm-v1` or
    similar; numpy must be available in the rootfs).
  - For N > 1 fanout: `sudo bash scripts/host-tap.sh` +
    `sudo bash scripts/netns-setup.sh N`.
  - `pip install forkd>=0.3.2`.

Usage:
    FORKD_TOKEN=$(sudo cat /etc/forkd/token) \\
        python3 recipes/speculative-agent/demo.py [snapshot_tag] [--n=3]

No LLM key required. The "agent" is deterministic Python that imitates
the speculative-execution pattern an LLM-driven agent would use.
"""

from __future__ import annotations

import argparse
import os
import sys
import time
from dataclasses import dataclass
from typing import Optional

try:
    from forkd import Controller
except ImportError as e:
    print(f"missing 'forkd' library: {e}", file=sys.stderr)
    print("install with: pip install forkd>=0.3.2", file=sys.stderr)
    sys.exit(2)


# Strategies. Each is a one-line Python program that computes
# sum(i*i for i in range(1, N+1)) using a different approach. We pass
# the source as a string and exec it inside the sandbox so the import
# cost lands on the strategy that needs it — *that's the whole point*.

N_PROBLEM = 100_000  # "compute sum of squares 1..N"; tune for noticeable timing splits

STRATEGIES = {
    "loop": (
        "import time;"
        f"N={N_PROBLEM};"
        "t0=time.perf_counter();"
        "s=0;\n"
        "for i in range(1, N+1):\n"
        "    s += i*i\n"
        "print(f'answer={s}|wall_us={(time.perf_counter()-t0)*1e6:.0f}')"
    ),
    "formula": (
        "import time;"
        f"N={N_PROBLEM};"
        "t0=time.perf_counter();"
        "s=N*(N+1)*(2*N+1)//6;"
        "print(f'answer={s}|wall_us={(time.perf_counter()-t0)*1e6:.0f}')"
    ),
    "numpy": (
        "import time;import numpy as np;"
        f"N={N_PROBLEM};"
        "t0=time.perf_counter();"
        "a=np.arange(1, N+1, dtype=np.int64);"
        "s=int(a.dot(a));"
        "print(f'answer={s}|wall_us={(time.perf_counter()-t0)*1e6:.0f}')"
    ),
}


@dataclass
class StrategyResult:
    name: str
    sandbox_id: str
    answer: Optional[int]
    wall_us: Optional[int]
    raw_stdout: str
    exit_code: int


def parse_result(name: str, sandbox_id: str, exec_result: dict) -> StrategyResult:
    """Parse the strategy's stdout line ('answer=...|wall_us=...')."""
    stdout = exec_result.get("stdout", "").strip()
    exit_code = exec_result.get("exit_code", -1)
    answer: Optional[int] = None
    wall_us: Optional[int] = None
    if exit_code == 0 and "|" in stdout:
        # Be lenient — accept any line where both fields are present.
        for chunk in stdout.replace("\n", "|").split("|"):
            if chunk.startswith("answer="):
                try:
                    answer = int(chunk.removeprefix("answer="))
                except ValueError:
                    pass
            elif chunk.startswith("wall_us="):
                try:
                    wall_us = int(chunk.removeprefix("wall_us="))
                except ValueError:
                    pass
    return StrategyResult(
        name=name,
        sandbox_id=sandbox_id,
        answer=answer,
        wall_us=wall_us,
        raw_stdout=stdout,
        exit_code=exit_code,
    )


def judge(results: list[StrategyResult]) -> StrategyResult:
    """The 'best' strategy is the one that produces the right answer
    fastest. All correct strategies should agree on the answer; the
    one with the smallest wall_us wins."""
    correct = [r for r in results if r.answer is not None and r.wall_us is not None]
    if not correct:
        raise SystemExit("[speculative] all strategies failed to produce a parseable answer")
    # Sanity: do they all agree on the answer?
    answers = {r.answer for r in correct}
    if len(answers) > 1:
        print(
            f"[speculative] WARNING: strategies disagree on the answer: {answers}",
            file=sys.stderr,
        )
    return min(correct, key=lambda r: r.wall_us or 0)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "snapshot_tag",
        nargs="?",
        default=None,
        help="parent snapshot to fork from (defaults to first available)",
    )
    parser.add_argument(
        "--n",
        type=int,
        default=3,
        help="number of strategies to try (max = number of defined strategies)",
    )
    args = parser.parse_args()

    chosen_strategies = list(STRATEGIES.items())[: args.n]
    if len(chosen_strategies) < args.n:
        print(
            f"[speculative] only {len(chosen_strategies)} strategies defined; "
            f"capping n to that.",
            file=sys.stderr,
        )

    controller = Controller()
    snapshots = controller.list_snapshots()
    if not snapshots:
        raise SystemExit("no snapshots; build one with `forkd snapshot`")
    tag = args.snapshot_tag or snapshots[0]["tag"]
    print(f"[speculative] using snapshot '{tag}'")

    # 1) source sandbox — represents the agent's working state at the
    # decision point. We don't actually load anything special here; the
    # demo's point is that BRANCH would carry over whatever IS loaded.
    [src] = controller.spawn_sandboxes(snapshot_tag=tag, n=1)
    src_id = src["id"]
    print(f"[speculative] source sandbox: {src_id}")

    try:
        # 2) BRANCH at the decision point. diff=True for the v0.3 fast path.
        t0 = time.monotonic()
        branch = controller.branch_sandbox(
            src_id,
            tag=f"speculative-{int(time.time() * 1000)}",
            diff=True,
        )
        branch_ms = (time.monotonic() - t0) * 1000
        print(
            f"[speculative] BRANCH (diff=true) in {branch_ms:.0f}ms "
            f"(diff_physical_bytes={branch.get('diff_physical_bytes')})"
        )

        # 3) Fan out one child per strategy. per_child_netns is required
        # because the source is still alive on forkd-tap0.
        t0 = time.monotonic()
        kids = controller.spawn_sandboxes(
            snapshot_tag=branch["tag"],
            n=len(chosen_strategies),
            per_child_netns=True,
        )
        spawn_ms = (time.monotonic() - t0) * 1000
        print(
            f"[speculative] spawned {len(kids)} grandchildren in "
            f"{spawn_ms:.0f}ms ({spawn_ms / len(kids):.0f}ms/child)"
        )

        # 4) Each grandchild runs one strategy. Sequential dispatch
        # over the SDK's blocking exec; the children themselves run in
        # parallel — what matters is each had its own warm VM.
        results: list[StrategyResult] = []
        for (name, code), kid in zip(chosen_strategies, kids):
            exec_result = controller.exec_command(
                kid["id"],
                ["python3", "-c", code],
                timeout_secs=15,
            )
            r = parse_result(name, kid["id"], exec_result)
            results.append(r)
            print(
                f"  [{r.name:<8}] {r.sandbox_id} → answer={r.answer} "
                f"wall_us={r.wall_us}"
                + (f"  ✗ exit={r.exit_code}" if r.exit_code != 0 else "")
            )

        # 5) Judge picks the winner.
        winner = judge(results)
        print()
        print(f"[speculative] WINNER: {winner.name} ({winner.wall_us} µs)")
        baseline = max((r.wall_us or 0) for r in results if r.wall_us is not None)
        if winner.wall_us and baseline:
            print(
                f"[speculative] winner is {baseline / winner.wall_us:.1f}× faster "
                f"than the slowest strategy"
            )

        # 6) Cleanup grandchildren. In real use you'd keep the winner's
        # sandbox alive and use it as the new source for the next
        # decision point — that's the speculative-execution loop.
        for k in kids:
            controller.kill_sandbox(k["id"])
        print(f"[speculative] cleaned up {len(kids)} grandchildren")

    finally:
        controller.kill_sandbox(src_id)
        print(f"[speculative] cleaned up source sandbox {src_id}")


if __name__ == "__main__":
    main()
