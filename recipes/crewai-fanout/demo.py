#!/usr/bin/env python3
"""crewai-fanout: a CrewAI crew where every agent runs its tool calls
inside its own dedicated forkd sandbox.

The point of this recipe: show CrewAI users a pattern they can't do
today without forkd. CrewAI's parallel execution shares one Python
process — agents step on each other's `sys.path`, environment, and
half-imported state. With forkd, each agent gets a fresh microVM
forked from a warmed parent snapshot in ~milliseconds.

What this script does:

  1. Provisions N forkd sandboxes from a single parent snapshot, one
     per agent. All N are forked from the same parent → all N inherit
     the parent's pre-warmed Python state (imports, package cache).
  2. Wraps each sandbox in a CrewAI-compatible BaseTool — `ForkdRun`.
  3. Builds a Crew of N "researcher" agents and assigns each one a
     different subproblem (compute fib(N), check primality, etc.).
  4. Runs the crew. Each agent calls its own ForkdRun tool, which
     exec's code inside that agent's sandbox. The agents are isolated
     end-to-end: a `del sys.modules["math"]` in one does not affect
     the others.
  5. Times spawn-vs-Docker so you can see the cold-start savings.

Prerequisites:
  - forkd-controller running locally (`forkd-controller serve ...`)
    with at least one Python-capable snapshot (e.g.
    `coding-agent-fork-prewarm-v1` or any rootfs from
    `recipes/python-numpy/`).
  - For fanout N>1 you also need per-child netns:
        sudo bash scripts/host-tap.sh
        sudo bash scripts/netns-setup.sh <N>
  - `pip install crewai forkd>=0.3.1`
  - One of: `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` for the LLM that
    drives the Crew. Without a key the script runs in `--dry-run`
    mode (prints the wiring, exits without calling the LLM).

Usage:
    FORKD_TOKEN=$(sudo cat /etc/forkd/token) \\
        python3 recipes/crewai-fanout/demo.py [snapshot_tag] [--n=3]

The point of this script is to be the smallest interesting CrewAI +
forkd integration that you can read in 5 minutes and adapt to your
own crew.
"""

from __future__ import annotations

import argparse
import os
import sys
import time
from dataclasses import dataclass
from typing import Any

try:
    from forkd import Controller
except ImportError as e:
    print(f"missing 'forkd' library: {e}", file=sys.stderr)
    print("install with: pip install forkd>=0.3.1", file=sys.stderr)
    sys.exit(2)


# ----------------------------------------------------------------------
# Forkd plumbing
# ----------------------------------------------------------------------


@dataclass
class SandboxHandle:
    """One sandbox, one agent. Cleaned up at end of run."""

    id: str
    snapshot_tag: str


def provision_sandboxes(
    controller: Controller, snapshot_tag: str, n: int, per_child_netns: bool
) -> tuple[list[SandboxHandle], float]:
    """Spawn N sandboxes from one parent snapshot. Returns (handles, seconds).

    The single `spawn_sandboxes` call is the v0.3 fast path — all N
    children forked from one shared memory image in one daemon round-
    trip. Compare this to `docker run` × N, which is what CrewAI users
    do today.
    """
    t0 = time.monotonic()
    raw = controller.spawn_sandboxes(
        snapshot_tag=snapshot_tag,
        n=n,
        per_child_netns=per_child_netns,
        prewarm=False,
    )
    elapsed = time.monotonic() - t0
    return [SandboxHandle(id=sb["id"], snapshot_tag=snapshot_tag) for sb in raw], elapsed


# ----------------------------------------------------------------------
# CrewAI tool wrapper
# ----------------------------------------------------------------------


def make_forkd_tool(controller: Controller, sandbox: SandboxHandle):
    """Build a CrewAI BaseTool that exec's code in `sandbox`.

    Imports `crewai_tools.BaseTool` lazily so this script can still
    print its dry-run plan even when CrewAI isn't installed (the
    forkd plumbing above is what readers care about; CrewAI wiring is
    just one of many possible bindings).
    """
    from crewai.tools import BaseTool  # type: ignore[import-not-found]
    from pydantic import Field  # type: ignore[import-not-found]

    sb_id = sandbox.id

    class ForkdRun(BaseTool):
        name: str = f"forkd_run_{sb_id[-6:]}"
        description: str = (
            "Execute a short Python expression inside an isolated forkd "
            "microVM. Input: a Python expression as a string. Output: "
            "stdout from the sandbox. Use this for any computation; do "
            "not run code in your own process — the answer there is not "
            "valid for the user."
        )
        # Pydantic v2 — pass sandbox id through as a field so it
        # persists into the tool description and the LLM sees it.
        sandbox_id: str = Field(default=sb_id)

        def _run(self, code: str) -> str:
            result = controller.exec_command(
                self.sandbox_id,
                ["python3", "-c", code],
                timeout_secs=30,
            )
            stdout = result.get("stdout", "")
            stderr = result.get("stderr", "")
            exit_code = result.get("exit_code", -1)
            if exit_code != 0:
                return f"[exit={exit_code}] stderr: {stderr}\nstdout: {stdout}"
            return stdout.strip() or "(no output)"

    return ForkdRun()


# ----------------------------------------------------------------------
# Crew builder
# ----------------------------------------------------------------------

# A small, deterministic set of subtasks. Real crews would source these
# from the user. We deliberately make each one trivial so the demo runs
# in seconds and the focus stays on the isolation pattern.
SUBTASKS = [
    "Compute the 25th Fibonacci number and explain the result.",
    "Check whether 9973 is prime. Show the divisor check you ran.",
    "Sort the list [4, 1, 8, 2, 9, 3] in descending order. Return the result.",
    "Sum the squares of integers 1..20. Return both the formula and the value.",
    "Reverse the string 'forkd-microvm' character-by-character. Return the result.",
]


def build_crew(tools, subtasks: list[str]):
    """Build a Crew of N agents each with its own ForkdRun tool.

    Imports CrewAI lazily so dry-run mode (no LLM key) still works
    without crewai installed.
    """
    from crewai import Agent, Crew, Process, Task  # type: ignore[import-not-found]

    n = len(tools)
    agents = []
    tasks = []
    for i in range(n):
        agent = Agent(
            role=f"researcher_{i}",
            goal=(
                f"Use your forkd sandbox tool to answer subtask {i}. "
                "You MUST call the tool — do not solve in your head."
            ),
            backstory=(
                "You are a precise computational researcher who delegates "
                "every numeric or string operation to a sandboxed Python "
                "interpreter. You never trust your own arithmetic."
            ),
            tools=[tools[i]],
            allow_delegation=False,
            verbose=False,
        )
        task = Task(
            description=subtasks[i],
            expected_output="A single short sentence stating the result.",
            agent=agent,
        )
        agents.append(agent)
        tasks.append(task)

    return Crew(agents=agents, tasks=tasks, process=Process.sequential, verbose=False)


# ----------------------------------------------------------------------
# Main
# ----------------------------------------------------------------------


def has_llm_key() -> bool:
    return any(os.environ.get(k) for k in ("ANTHROPIC_API_KEY", "OPENAI_API_KEY"))


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "snapshot_tag",
        nargs="?",
        default=None,
        help="parent snapshot to fork from (defaults to first available)",
    )
    parser.add_argument("--n", type=int, default=3, help="number of agents/sandboxes")
    parser.add_argument(
        "--per-child-netns",
        action="store_true",
        default=True,
        help="put each child in its own netns (required for n>1; default true)",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="skip CrewAI/LLM, just provision + tear down sandboxes",
    )
    args = parser.parse_args()

    if args.n > len(SUBTASKS):
        print(
            f"only {len(SUBTASKS)} subtasks defined; capping n to that.",
            file=sys.stderr,
        )
        args.n = len(SUBTASKS)

    # Controller reads FORKD_URL / FORKD_TOKEN from env by default.
    controller = Controller()

    snapshots = controller.list_snapshots()
    if not snapshots:
        raise SystemExit(
            "no snapshots on the controller; build one with `forkd snapshot`"
        )
    tag = args.snapshot_tag or snapshots[0]["tag"]
    print(f"[crewai-fanout] using snapshot '{tag}'")

    handles, spawn_secs = provision_sandboxes(
        controller, tag, args.n, args.per_child_netns
    )
    print(
        f"[crewai-fanout] spawned {len(handles)} sandboxes in "
        f"{spawn_secs * 1000:.1f}ms "
        f"({spawn_secs * 1000 / args.n:.1f}ms/child)"
    )
    for h in handles:
        print(f"  - {h.id}")

    if args.dry_run or not has_llm_key():
        reason = "--dry-run" if args.dry_run else "no LLM key in env"
        print(f"[crewai-fanout] skipping CrewAI run ({reason}); plan:")
        for i, (h, task) in enumerate(zip(handles, SUBTASKS)):
            print(f"  agent[{i}] → sandbox {h.id} → task: {task!s}")
    else:
        tools = [make_forkd_tool(controller, h) for h in handles]
        crew = build_crew(tools, SUBTASKS[: args.n])
        print(f"[crewai-fanout] kicking off crew with {args.n} agents")
        t0 = time.monotonic()
        result = crew.kickoff()
        kickoff_secs = time.monotonic() - t0
        print(f"[crewai-fanout] crew done in {kickoff_secs:.2f}s")
        print("[crewai-fanout] crew result:")
        print(str(result))

    # Cleanup
    for h in handles:
        try:
            controller.kill_sandbox(h.id)
        except Exception as e:  # noqa: BLE001  best-effort cleanup
            print(
                f"[crewai-fanout] warning: failed to kill {h.id}: {e}",
                file=sys.stderr,
            )
    print(f"[crewai-fanout] cleaned up {len(handles)} sandboxes")


if __name__ == "__main__":
    main()
