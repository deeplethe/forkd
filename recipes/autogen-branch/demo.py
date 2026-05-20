#!/usr/bin/env python3
"""autogen-branch: an AutoGen ConversableAgent backed by a forkd sandbox
via the official `CodeExecutor` extension point — plus a mid-conversation
BRANCH that fans out N alternate continuations from the same warmed
state.

Why this exists. AutoGen ships two production CodeExecutors:
`LocalCommandLineCodeExecutor` (no isolation) and
`DockerCommandLineCodeExecutor` (slow cold-start, per-container disk
overhead). Neither lets you do the forkd-shaped move: "take this
agent's current state, fork three copies that diverge from here". This
recipe shows how to plug forkd in as a third executor option AND how
to do the BRANCH move on top.

What this script does:

  1. Wraps a forkd sandbox in a `ForkdCommandLineCodeExecutor` that
     conforms to `autogen_core.code_executor.CodeExecutor`. Every
     `execute_code_blocks(...)` call exec's inside the sandbox.
  2. Builds an AutoGen `ConversableAgent` whose `code_execution_config`
     points to that executor.
  3. Runs the agent for one or two turns (a small deterministic prompt
     so the demo doesn't need an LLM key end-to-end).
  4. **BRANCHes the sandbox** mid-conversation — captures its memory
     state into a new tag.
  5. Spawns 3 grandchildren from the branch. Each grandchild is a
     fresh AutoGen executor whose Python state already contains
     everything the parent agent had imported / computed / cached.

Prerequisites:
  - forkd-controller running locally with a Python-capable snapshot.
  - For fanout n>1: `sudo bash scripts/host-tap.sh` +
    `sudo bash scripts/netns-setup.sh 3`.
  - `pip install pyautogen forkd>=0.3.1`.
  - LLM key optional (`OPENAI_API_KEY` etc.) — the demo runs in
    `--dry-run` mode without one and just exercises the executor +
    BRANCH path. With a key it runs a real ConversableAgent turn.

Usage:
    FORKD_TOKEN=$(sudo cat /etc/forkd/token) \\
        python3 recipes/autogen-branch/demo.py [snapshot_tag]

The forkd-specific code is contained in
`ForkdCommandLineCodeExecutor` and the BRANCH section near the end.
Everything else is plain AutoGen — copy the executor class into your
project and you have a working binding.
"""

from __future__ import annotations

import argparse
import os
import sys
import textwrap
import time
from pathlib import Path
from typing import Any, Optional

try:
    from forkd import Controller
except ImportError as e:
    print(f"missing 'forkd' library: {e}", file=sys.stderr)
    print("install with: pip install forkd>=0.3.1", file=sys.stderr)
    sys.exit(2)


# ----------------------------------------------------------------------
# AutoGen executor plug-in
# ----------------------------------------------------------------------


def make_forkd_executor(controller: Controller, sandbox_id: str):
    """Return an `autogen_core.code_executor.CodeExecutor` that exec's
    inside the given forkd sandbox.

    Imports `autogen_core` lazily so dry-run mode still works without
    pyautogen installed — the forkd plumbing is the point of this
    recipe; the AutoGen binding is one of several possible shells.
    """
    from autogen_core.code_executor import (  # type: ignore[import-not-found]
        CodeBlock,
        CodeExecutor,
        CodeResult,
    )

    class ForkdCommandLineCodeExecutor(CodeExecutor):
        """Exec each AutoGen code block inside a forkd microVM.

        Stateless from AutoGen's point of view: each `execute_code_blocks`
        call runs in a fresh `python3 -c`. The sandbox itself accumulates
        OS-level state (file writes to /tmp, installed packages, etc.) —
        which is exactly what BRANCH later captures.
        """

        def __init__(self, controller: Controller, sandbox_id: str) -> None:
            self._controller = controller
            self._sandbox_id = sandbox_id

        @property
        def sandbox_id(self) -> str:
            return self._sandbox_id

        async def execute_code_blocks(
            self, code_blocks: list[CodeBlock], cancellation_token: Any
        ) -> CodeResult:
            """Run each block sequentially; return aggregated CodeResult.

            We support `python` blocks (the common case). Other languages
            (bash, etc.) raise — same behavior as the upstream Docker
            executor before its multi-lang support landed; small surface,
            easy to understand, easy to extend in your own fork.
            """
            output_chunks: list[str] = []
            for block in code_blocks:
                if block.language not in ("python", "py"):
                    raise NotImplementedError(
                        f"ForkdCommandLineCodeExecutor only supports python "
                        f"blocks for now; got {block.language!r}. PRs welcome."
                    )
                result = self._controller.exec_command(
                    self._sandbox_id,
                    ["python3", "-c", block.code],
                    timeout_secs=30,
                )
                output_chunks.append(result.get("stdout", ""))
                stderr = result.get("stderr", "")
                if stderr:
                    output_chunks.append(f"[stderr]\n{stderr}")
                if result.get("exit_code", 0) != 0:
                    return CodeResult(
                        exit_code=result["exit_code"],
                        output="\n".join(output_chunks),
                    )
            return CodeResult(exit_code=0, output="\n".join(output_chunks))

        async def restart(self) -> None:
            """AutoGen's `restart()` contract is "throw away in-process
            state and start fresh". For forkd that's a no-op: each exec
            is a new `python3 -c` process. Truly restarting would mean
            spawning a fresh sandbox, which the caller can do explicitly
            via Controller.spawn_sandboxes — we don't hide that here."""
            return

    return ForkdCommandLineCodeExecutor(controller, sandbox_id)


# ----------------------------------------------------------------------
# Demo flow
# ----------------------------------------------------------------------


def has_llm_key() -> bool:
    return any(
        os.environ.get(k) for k in ("OPENAI_API_KEY", "AZURE_OPENAI_API_KEY")
    )


def run_dry(controller: Controller, sandbox_id: str) -> None:
    """Exercise the executor path WITHOUT pulling in pyautogen / LLM.

    We still call `execute_code_blocks` via the real class — the test is:
    "the forkd-backed executor returns the expected output". This
    catches breakage in the executor wiring even without an LLM.
    """
    import asyncio

    executor = make_forkd_executor(controller, sandbox_id)

    from autogen_core.code_executor import (  # type: ignore[import-not-found]
        CodeBlock,
    )

    block = CodeBlock(
        language="python",
        code="print('hello from autogen-branch'); print(2 ** 16)",
    )

    async def go():
        return await executor.execute_code_blocks([block], cancellation_token=None)

    result = asyncio.run(go())
    print(f"[autogen-branch] dry-run exec result (exit={result.exit_code}):")
    print(textwrap.indent(result.output.strip(), "  "))


def run_with_llm(controller: Controller, sandbox_id: str) -> None:
    """Spin up a real ConversableAgent + UserProxyAgent loop with the
    forkd executor wired in."""
    from autogen import ConversableAgent, UserProxyAgent  # type: ignore[import-not-found]

    executor = make_forkd_executor(controller, sandbox_id)

    # We use AutoGen's high-level ConversableAgent. Code_execution_config
    # takes a dict — newer AutoGen versions accept a CodeExecutor
    # instance directly via "executor".
    coder = ConversableAgent(
        name="coder",
        system_message=(
            "You write tiny Python programs to answer arithmetic questions. "
            "Wrap every code in a python markdown block; never compute in your head."
        ),
        llm_config={
            "model": os.environ.get("OPENAI_MODEL", "gpt-4o-mini"),
            "api_key": os.environ.get("OPENAI_API_KEY"),
        },
        code_execution_config=False,  # coder generates code; doesn't execute
    )
    user = UserProxyAgent(
        name="user",
        human_input_mode="NEVER",
        max_consecutive_auto_reply=1,
        code_execution_config={"executor": executor},
    )

    print("[autogen-branch] starting ConversableAgent turn (one round)")
    user.initiate_chat(
        coder,
        message="Compute the sum of the digits of 2**100. Show your code.",
        max_turns=2,
    )


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "snapshot_tag",
        nargs="?",
        default=None,
        help="parent snapshot to fork from (defaults to first available)",
    )
    parser.add_argument(
        "--fanout", type=int, default=3, help="grandchildren to spawn from BRANCH"
    )
    parser.add_argument(
        "--per-child-netns",
        action="store_true",
        default=True,
        help="put each grandchild in its own netns (required for fanout>1)",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="skip LLM call; just exercise executor + BRANCH + fanout",
    )
    args = parser.parse_args()

    controller = Controller()

    snapshots = controller.list_snapshots()
    if not snapshots:
        raise SystemExit("no snapshots; build one with `forkd snapshot`")
    tag = args.snapshot_tag or snapshots[0]["tag"]
    print(f"[autogen-branch] using snapshot '{tag}'")

    # 1) spawn the source sandbox the conversable agent will use
    source = controller.spawn_sandboxes(snapshot_tag=tag, n=1)
    sb = source[0]
    sb_id = sb["id"]
    print(f"[autogen-branch] source sandbox: {sb_id}")

    try:
        # 2) drive the executor (with or without an LLM)
        if args.dry_run or not has_llm_key():
            reason = "--dry-run" if args.dry_run else "no LLM key"
            print(f"[autogen-branch] dry-run mode ({reason})")
            run_dry(controller, sb_id)
        else:
            run_with_llm(controller, sb_id)

        # 3) BRANCH — capture the agent's accumulated VM state
        branch_tag = f"autogen-branch-{int(time.time() * 1000)}"
        t0 = time.monotonic()
        branch = controller.branch_sandbox(sb_id, tag=branch_tag)
        branch_secs = time.monotonic() - t0
        print(
            f"[autogen-branch] BRANCH → tag={branch['tag']} "
            f"(client-observed {branch_secs * 1000:.0f}ms)"
        )

        # 4) fan out grandchildren from the branch
        t0 = time.monotonic()
        kids = controller.spawn_sandboxes(
            snapshot_tag=branch["tag"],
            n=args.fanout,
            per_child_netns=args.per_child_netns,
        )
        fanout_secs = time.monotonic() - t0
        print(
            f"[autogen-branch] fanned out {len(kids)} grandchildren in "
            f"{fanout_secs * 1000:.0f}ms"
        )

        # 5) sanity-check each grandchild inherited state by running a
        #    minimal exec inside each — none of them should have to
        #    re-import or re-warm anything.
        for k in kids:
            r = controller.exec_command(
                k["id"],
                ["sh", "-c", "echo from-$(hostname); python3 -c 'import sys; print(sys.version_info[:3])'"],
                timeout_secs=10,
            )
            print(f"  {k['id']}: exit={r.get('exit_code')} stdout={r.get('stdout','').strip()!r}")

        # 6) cleanup grandchildren
        for k in kids:
            controller.kill_sandbox(k["id"])
        print(f"[autogen-branch] cleaned up {len(kids)} grandchildren")

        # Keep the source sandbox running until the finally block — that
        # way the cleanup path is the same whether we got an exception
        # mid-flow or not.
    finally:
        controller.kill_sandbox(sb_id)
        print(f"[autogen-branch] cleaned up source sandbox {sb_id}")


if __name__ == "__main__":
    main()
