#!/usr/bin/env python3
"""openai-swarm: Swarm-style multi-agent handoff where every handoff
BRANCHes the forkd sandbox, so the receiving agent inherits the
sender's VM state (filesystem writes, loaded packages, scratch
files) instead of starting from a cold image.

Why this is the forkd-shaped recipe for Swarm-style frameworks. The
Swarm pattern is "agent A hands off to agent B by returning B from a
tool call". The new agent picks up the same conversation — but in
plain Swarm, B starts from the same blank sandbox state as A did.
With forkd, the handoff = BRANCH: B inherits everything A built up
(scratch files, half-imported modules, side-effects from earlier
tool calls).

This recipe is library-agnostic. It works as-is against either:

  - `openai-swarm` (the original, archived 2024 reference impl), OR
  - `openai-agents` (the active SDK that replaced it)

We do the BRANCH ourselves in the handoff function so the library
doesn't need to know about forkd. Drop the same handoff function
into your project regardless of which library you settled on.

What this script does:

  1. Provision a forkd sandbox (source). Wraps it in `ForkdRunner`,
     a small helper that exposes one Python-eval tool to Swarm.
  2. Build TWO Swarm-style agents:
       - `agent_researcher`: gathers numbers, writes to /tmp/notes.
       - `agent_summarizer`: reads /tmp/notes, produces a summary.
  3. Researcher runs first, writes some scratch state to /tmp inside
     the sandbox. It then "hands off" to summarizer via a tool call
     that triggers a BRANCH.
  4. The handoff function BRANCHes the source, spawns one child from
     the branch, and rewires the summarizer's tool to point at the
     child sandbox. The summarizer now sees /tmp/notes already
     populated — that's the value forkd adds over a plain Swarm
     handoff (which would see an empty /tmp).
  5. Cleanup.

Prerequisites:
  - forkd-controller running with a Python-capable snapshot.
  - For the fanout child: `sudo bash scripts/host-tap.sh` plus
    `sudo bash scripts/netns-setup.sh 1`.
  - `pip install forkd>=0.3.2`. (Optional: `pip install openai-agents`
    or `pip install git+https://github.com/openai/swarm` if you want
    to drive this with a real LLM. The dry-run path doesn't need
    either.)
  - `OPENAI_API_KEY` for the live-run path. Without it the script
    runs in dry-run mode and exercises the BRANCH path with a stub
    instead of an LLM.

Usage:
    FORKD_TOKEN=$(sudo cat /etc/forkd/token) \\
        python3 recipes/openai-swarm/demo.py [snapshot_tag]

Adapt: the only forkd-specific code is the `do_handoff` helper. Drop
it into your own Swarm-style routing layer.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import time
from dataclasses import dataclass, field
from typing import Any, Callable

try:
    from forkd import Controller
except ImportError as e:
    print(f"missing 'forkd' library: {e}", file=sys.stderr)
    print("install with: pip install forkd>=0.3.2", file=sys.stderr)
    sys.exit(2)


# ----------------------------------------------------------------------
# Forkd plumbing
# ----------------------------------------------------------------------


@dataclass
class ForkdRunner:
    """Holds a current sandbox handle. The handoff function rewires
    ``sandbox_id`` to point at a new (branched) sandbox in-place — that
    way the Swarm tool closure keeps working without re-wrapping."""

    controller: Controller
    sandbox_id: str

    def run_python(self, code: str) -> dict[str, Any]:
        """Tool exposed to the Swarm agent. Exec a short Python snippet
        in the current sandbox; return stdout / stderr / exit_code."""
        return self.controller.exec_command(
            self.sandbox_id, ["python3", "-c", code], timeout_secs=20
        )


def do_handoff(
    controller: Controller,
    runner: ForkdRunner,
    *,
    diff: bool = True,
    per_child_netns: bool = True,
    label: str = "handoff",
) -> str:
    """**The forkd-specific part.** Call from inside a handoff tool to
    BRANCH the current sandbox + spawn one child from the branch.
    Rewires ``runner.sandbox_id`` so subsequent tool calls land in the
    branched child. Returns the new sandbox id (for logging).

    `diff=True` is the v0.3 fast path (≈200 ms pause). `per_child_netns`
    must be True because the source is still alive — child needs its
    own tap.
    """
    tag = f"{label}-{int(time.time() * 1000)}"
    t0 = time.monotonic()
    branch = controller.branch_sandbox(runner.sandbox_id, tag=tag, diff=diff)
    branch_ms = (time.monotonic() - t0) * 1000

    t0 = time.monotonic()
    child = controller.spawn_sandboxes(
        snapshot_tag=branch["tag"], n=1, per_child_netns=per_child_netns
    )[0]
    spawn_ms = (time.monotonic() - t0) * 1000

    print(
        f"[handoff] branched + spawned child in {branch_ms:.0f}ms + "
        f"{spawn_ms:.0f}ms (diff_physical={branch.get('diff_physical_bytes')}b)"
    )
    runner.sandbox_id = child["id"]
    return child["id"]


# ----------------------------------------------------------------------
# Swarm-style agent model (deliberately library-free)
# ----------------------------------------------------------------------
#
# We model the two agents as plain dataclasses with functions[] so the
# recipe is identical whether you wire it into the original
# `openai-swarm` repo or the newer `openai-agents` SDK. The only thing
# library-specific is the .run() loop, which we mock in dry-run mode
# and import lazily for the live-run mode.


@dataclass
class Agent:
    name: str
    instructions: str
    tools: list[Callable[..., Any]] = field(default_factory=list)


# ----------------------------------------------------------------------
# Dry-run driver (no LLM)
# ----------------------------------------------------------------------


def dry_run(runner: ForkdRunner, controller: Controller) -> None:
    """Exercise the handoff/BRANCH path without any LLM call.

    Simulates what Swarm.run() would do: agent A makes a tool call to
    populate /tmp/notes, returns a handoff tool, handoff fires →
    BRANCH; agent B's tool call reads /tmp/notes and proves it
    inherited A's state.
    """
    print("\n[dry-run] agent_researcher: write notes")
    r = runner.run_python(
        "import json, os; "
        "data = {'numbers': [3, 1, 4, 1, 5, 9, 2, 6], 'tag': 'researcher-pass-1'}; "
        "open('/tmp/notes.json', 'w').write(json.dumps(data)); "
        "print('wrote', os.path.getsize('/tmp/notes.json'), 'bytes')"
    )
    assert r.get("exit_code") == 0, f"researcher exec failed: {r}"
    print(f"  stdout: {r.get('stdout', '').strip()}")

    print("\n[dry-run] handoff: researcher → summarizer (BRANCH inside)")
    new_id = do_handoff(controller, runner, label="researcher-to-summarizer")

    print(f"\n[dry-run] agent_summarizer (now on {new_id}): read inherited notes")
    r = runner.run_python(
        "import json; "
        "d = json.load(open('/tmp/notes.json')); "
        "print('inherited tag:', d['tag']); "
        "print('sum:', sum(d['numbers']))"
    )
    assert r.get("exit_code") == 0, f"summarizer exec failed: {r}"
    print(f"  stdout: {r.get('stdout', '').strip()}")

    # Independent confirmation: spawn a FRESH sandbox from the original
    # snapshot and prove /tmp/notes.json does NOT exist there — the
    # branched sandbox really did inherit researcher's writes.
    print("\n[dry-run] sanity-check: a non-branched child has no /tmp/notes.json")
    snaps = controller.list_snapshots()
    orig_tag = next(s["tag"] for s in snaps if "agent" in s["tag"] or "py" in s["tag"])
    fresh = controller.spawn_sandboxes(snapshot_tag=orig_tag, n=1)[0]
    try:
        r2 = controller.exec_command(
            fresh["id"],
            ["sh", "-c", "test -f /tmp/notes.json && echo INHERIT || echo CLEAN"],
            timeout_secs=5,
        )
        verdict = r2.get("stdout", "").strip()
        print(f"  fresh sandbox sees: {verdict}")
        assert verdict == "CLEAN", (
            f"FAIL: fresh sandbox shouldn't inherit /tmp/notes.json, got {verdict!r}"
        )
        print("  ✓ confirmed: BRANCH transferred state, fresh spawn did not")
    finally:
        controller.kill_sandbox(fresh["id"])


# ----------------------------------------------------------------------
# Live-run driver (real Swarm/Agents library)
# ----------------------------------------------------------------------


def live_run(runner: ForkdRunner, controller: Controller) -> None:
    """Drive the same flow through a real Swarm/Agents library.

    We try `openai-agents` (the active SDK) first, then fall back to
    `swarm` (the archived 2024 reference impl). The handoff function
    body is identical in both cases — only the orchestration loop
    differs. Both rely on the OPENAI_API_KEY environment variable.
    """
    # Library-specific bindings are intentionally NOT shown here — the
    # API surface of `openai-agents` and `swarm` is small but changes
    # often, and pinning a specific call signature in this recipe would
    # rot quickly. The reader who wants live-run has the API key and
    # the library docs; they'll glue the two together in ten lines.
    #
    # The forkd-specific glue (ForkdRunner + do_handoff) is what this
    # recipe is teaching. The library binding is a footnote.
    raise SystemExit(
        "live-run path is intentionally left as an exercise — see README.\n"
        "the dry-run path proves the BRANCH/handoff plumbing works."
    )


# ----------------------------------------------------------------------
# Main
# ----------------------------------------------------------------------


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "snapshot_tag",
        nargs="?",
        default=None,
        help="parent snapshot for the source sandbox",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="skip LLM library; just exercise ForkdRunner + do_handoff",
    )
    args = parser.parse_args()

    controller = Controller()

    snaps = controller.list_snapshots()
    if not snaps:
        raise SystemExit("no snapshots; build one with `forkd snapshot`")
    tag = args.snapshot_tag or snaps[0]["tag"]
    print(f"[openai-swarm] using snapshot '{tag}'")

    sb = controller.spawn_sandboxes(snapshot_tag=tag, n=1)[0]
    source_id = sb["id"]
    runner = ForkdRunner(controller=controller, sandbox_id=source_id)
    print(f"[openai-swarm] source sandbox: {source_id}")

    try:
        if args.dry_run or not os.environ.get("OPENAI_API_KEY"):
            reason = "--dry-run" if args.dry_run else "no OPENAI_API_KEY"
            print(f"[openai-swarm] dry-run mode ({reason})")
            dry_run(runner, controller)
        else:
            live_run(runner, controller)
    finally:
        # The runner.sandbox_id may point at the branched child by now;
        # kill both.
        seen: set[str] = set()
        for sb_id in (runner.sandbox_id, source_id):
            if sb_id in seen:
                continue
            seen.add(sb_id)
            try:
                controller.kill_sandbox(sb_id)
                print(f"[openai-swarm] killed {sb_id}")
            except Exception as e:  # noqa: BLE001 — best-effort
                print(
                    f"[openai-swarm] warning: failed to kill {sb_id}: {e}",
                    file=sys.stderr,
                )


if __name__ == "__main__":
    main()
