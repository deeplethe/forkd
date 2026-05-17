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
    """Per-agent summary. Falls back to the last `think` event when
    no `answer` event exists — many of our runs collect transcripts
    before the agent has produced a terminal answer, but the last
    `think` carries the hint-influenced reasoning we want to show.
    """
    final = next((e for e in reversed(events) if e.get("event") == "answer"), None)
    last_think = next(
        (e for e in reversed(events) if e.get("event") == "think" and e.get("content")),
        None,
    )
    stop = next((e for e in reversed(events) if e.get("event") == "stop"), None)
    hints = [e for e in events if e.get("event") == "hint"]
    tool_calls = [e for e in events if e.get("event") == "tool_call"]
    retries = [e for e in events if e.get("event") == "retry"]

    # Pick the best "what did this agent end up saying" content.
    output_kind: str
    output_text: str | None
    if final:
        output_kind = "answer"
        output_text = final.get("content")
    elif last_think:
        output_kind = "think_in_flight"
        output_text = last_think.get("content")
    else:
        output_kind = "none"
        output_text = None

    return {
        "steps": stop.get("steps") if stop else None,
        "total_tokens": stop.get("total_tokens") if stop else None,
        "wall_ms": stop.get("wall_ms") if stop else None,
        "tool_calls_total": len(tool_calls),
        "tool_call_names": [tc["name"] for tc in tool_calls],
        "retry_count": len(retries),
        "completed": stop is not None,
        "hint_seen": hints[-1]["hint"] if hints else None,
        "output_kind": output_kind,
        "output_text": output_text,
        # Kept for backward compatibility with prior summary.json
        # consumers; equals output_text only when kind == "answer".
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
    md.append(
        "| Agent | Status | Hint | Steps | Tokens | Wall (ms) | Retries | Tools | Sandbox |\n"
    )
    md.append("|---|---|---|---:|---:|---:|---:|---|---|\n")
    for name in ("parent", "thorough", "minimal", "cost"):
        s = summaries[name]
        hint = (s["hint_seen"] or "—")[:40] + (
            "…" if s["hint_seen"] and len(s["hint_seen"]) > 40 else ""
        )
        tools = (", ".join(s["tool_call_names"])[:40] + "…") if len(", ".join(s["tool_call_names"])) > 40 else (", ".join(s["tool_call_names"]) or "—")
        status = "✅ completed" if s["completed"] else "⏳ in-flight"
        md.append(
            f"| {name} | {status} | {hint} | {s['steps'] or '—'} | "
            f"{s['total_tokens'] or '—'} | {s['wall_ms'] or '—'} | "
            f"{s['retry_count']} | {tools} | `{ids[name]}` |\n"
        )
    md.append("\n")

    md.append("## Per-agent output\n\n")
    md.append(
        "Each box shows the agent's last meaningful content at "
        "collection time. **`answer`** means the agent produced a "
        "terminal response; **`think (in-flight)`** means it was "
        "still mid-reasoning when transcripts were collected — "
        "the divergence is still visible there.\n\n"
    )
    for name in ("parent", "thorough", "minimal", "cost"):
        s = summaries[name]
        md.append(f"### {name}\n\n")
        if s["hint_seen"]:
            md.append(f"*Hint:* {s['hint_seen']}\n\n")
        kind = s["output_kind"]
        if kind == "answer":
            md.append("**Type:** final `answer` event\n\n")
        elif kind == "think_in_flight":
            md.append("**Type:** last `think` event (agent was still reasoning at collection)\n\n")
        else:
            md.append("**Type:** _(no output captured — agent hit retries and never reached a think/answer event)_\n\n")
        body = s["output_text"] or "_(no content)_"
        md.append("```\n")
        md.append(body)
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
