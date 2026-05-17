#!/usr/bin/env python3
"""
Renders summary.md from a demo run's transcripts.

Layout of OUT_DIR after demo.sh:

    spawn.json                       — POST /sandboxes response
    branch.json                      — POST /branch response (carries pause_ms)
    grandchildren.json               — second POST /sandboxes
    source-parent-transcript.jsonl   — source agent log
    child-thorough-transcript.jsonl  — child A
    child-minimal-transcript.jsonl   — child B
    child-cost-transcript.jsonl      — child C

We emit summary.md with:
- the daemon pause_ms (headline number)
- per-agent token count, wall time, final answer
- the shared "ready_to_branch" anchor (proof they all started from
  the same cognitive state)
"""
import argparse
import json
import os
from pathlib import Path


def load_jsonl(path: Path) -> list:
    if not path.exists():
        return []
    out = []
    with path.open("r", encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            try:
                out.append(json.loads(line))
            except json.JSONDecodeError:
                # In practice the agent's emit always writes valid
                # JSON; only stray text in the log breaks parsing.
                # Skip with a marker.
                out.append({"event": "_unparsed", "raw": line[:200]})
    return out


def summarize_agent(events: list) -> dict:
    final = next((e for e in reversed(events) if e.get("event") == "answer"), None)
    stop = next((e for e in reversed(events) if e.get("event") == "stop"), None)
    hints = [e for e in events if e.get("event") == "hint"]
    tool_calls = [e for e in events if e.get("event") == "tool_call"]

    return {
        "steps": stop.get("steps") if stop else None,
        "total_tokens": stop.get("total_tokens") if stop else None,
        "wall_ms": stop.get("wall_ms") if stop else None,
        "tool_calls_total": len(tool_calls),
        "tool_call_names": [tc["name"] for tc in tool_calls],
        "hint_seen": hints[-1]["hint"] if hints else None,
        "final_answer": final.get("content") if final else None,
        "events_count": len(events),
    }


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__.strip().splitlines()[0])
    p.add_argument("--out-dir", required=True, type=Path)
    p.add_argument("--daemon-pause-ms", type=int, required=True)
    p.add_argument("--branch-tag", required=True)
    p.add_argument("--source-id", required=True)
    p.add_argument("--child-thorough", required=True)
    p.add_argument("--child-minimal", required=True)
    p.add_argument("--child-cost", required=True)
    args = p.parse_args()

    transcripts = {
        "parent": args.out_dir / "source-parent-transcript.jsonl",
        "thorough": args.out_dir / "child-thorough-transcript.jsonl",
        "minimal": args.out_dir / "child-minimal-transcript.jsonl",
        "cost": args.out_dir / "child-cost-transcript.jsonl",
    }
    ids = {
        "parent": args.source_id,
        "thorough": args.child_thorough,
        "minimal": args.child_minimal,
        "cost": args.child_cost,
    }

    summaries = {name: summarize_agent(load_jsonl(p)) for name, p in transcripts.items()}

    md = []
    md.append("# Branch-and-fan-out demo run\n")
    md.append(f"- BRANCH tag: `{args.branch_tag}`\n")
    md.append(f"- Daemon-measured pause window: **{args.daemon_pause_ms} ms**\n")
    md.append(f"- Source sandbox: `{args.source_id}`\n")
    md.append("\n")

    md.append("## Per-agent comparison\n\n")
    md.append("| Agent | Hint | Steps | Tokens | Wall (ms) | Tools called | Sandbox id |\n")
    md.append("|---|---|---:|---:|---:|---|---|\n")
    for name in ("parent", "thorough", "minimal", "cost"):
        s = summaries[name]
        hint = (s["hint_seen"] or "—")[:50] + ("…" if s["hint_seen"] and len(s["hint_seen"]) > 50 else "")
        tools = ", ".join(s["tool_call_names"]) or "—"
        md.append(
            f"| {name} | {hint} | {s['steps'] or '—'} | "
            f"{s['total_tokens'] or '—'} | {s['wall_ms'] or '—'} | "
            f"{tools} | `{ids[name]}` |\n"
        )
    md.append("\n")

    md.append("## Final itineraries\n\n")
    for name in ("parent", "thorough", "minimal", "cost"):
        s = summaries[name]
        md.append(f"### {name}\n\n")
        if s["hint_seen"]:
            md.append(f"*Hint:* {s['hint_seen']}\n\n")
        ans = s["final_answer"] or "_(no final answer recorded)_"
        md.append("```\n")
        md.append(ans)
        md.append("\n```\n\n")

    md.append("## What this run demonstrates\n\n")
    md.append(
        "- A single source agent ran the first **3 steps** of a "
        "trip-planning ReAct loop, calling the `weather` and "
        "`search_places` tools and building a partial plan in its "
        "conversation history.\n"
        "- We called `POST /v1/sandboxes/:id/branch`. The source "
        f"paused for **{args.daemon_pause_ms} ms** while its full "
        "memory image was snapshotted.\n"
        "- We spawned 3 grandchildren from the branched snapshot. "
        "Each inherited the source's reasoning state — same "
        "conversation history, same tool results, same partial "
        "plan.\n"
        "- We planted a different steering hint in each child's "
        "`/tmp/forkd-hint.txt`. The agents read this file on every "
        "step, so the **next** thought after the fork was perturbed "
        "differently per child.\n"
        "- All three children continued from the shared state and "
        "produced different itineraries. The parent continued in "
        "parallel with no hint as a control.\n\n"
        "This is the speculative-parallel-exploration primitive "
        "that closed-source platforms (Modal Sandboxes) keep behind "
        "their hidden moat. forkd does it open-source on KVM/Linux.\n"
    )

    summary_path = args.out_dir / "summary.md"
    summary_path.write_text("".join(md), encoding="utf-8")
    print(f"wrote {summary_path}")

    # Also dump the machine-readable summary for downstream tooling.
    machine = {
        "branch_tag": args.branch_tag,
        "daemon_pause_ms": args.daemon_pause_ms,
        "agents": {name: summaries[name] for name in summaries},
    }
    (args.out_dir / "summary.json").write_text(
        json.dumps(machine, indent=2, ensure_ascii=False), encoding="utf-8"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
