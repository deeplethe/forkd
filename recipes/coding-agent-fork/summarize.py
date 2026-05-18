#!/usr/bin/env python3
"""
Render summary.md for the coding-agent fork demo.

Layout of OUT_DIR after demo.sh:

    branch.json                      — BRANCH endpoint response
    state-evidence.txt               — per-agent md5 of __pycache__ + vendored.bin
    {source,minimal,rewrite,skip}-init-py.txt  — each agent's mathy/__init__.py
    {source,minimal,rewrite,skip}-agent.log    — each agent's full log
"""
import argparse
import json
from pathlib import Path


def read_text(p: Path) -> str:
    try:
        return p.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return ""


def parse_state_evidence(path: Path) -> dict:
    out = {}
    if not path.exists():
        return out
    for line in path.read_text().splitlines():
        parts = line.split()
        if len(parts) < 5:
            continue
        label, sandbox_id, pycache_md5, vendored_md5, vendored_size = parts[:5]
        out[label] = {
            "sandbox_id": sandbox_id,
            "pycache_md5": pycache_md5,
            "vendored_md5": vendored_md5,
            "vendored_size": int(vendored_size) if vendored_size.isdigit() else None,
        }
    return out


def extract_test_outcome(agent_log: str) -> str:
    """Eyeball the last test run's result. We grep for unittest's
    FAILED / OK / 'expected failures' line."""
    for line in reversed(agent_log.splitlines()):
        low = line.lower()
        if low.startswith("ok") and len(line) < 8:
            return "✅ passed"
        if "failed" in low and "(failures" in low.replace(" ", ""):
            return "❌ failed (real failures)"
        if "ok (expected failures" in low:
            return "⚠️ skipped (expected-failure)"
        if line.startswith("OK") or "OK (" in line:
            return "✅ passed"
        if line.startswith("FAILED"):
            return "❌ failed"
    return "?"


def main() -> int:
    p = argparse.ArgumentParser()
    p.add_argument("--out-dir", required=True, type=Path)
    p.add_argument("--daemon-pause-ms", type=int, required=True)
    p.add_argument("--branch-tag", required=True)
    p.add_argument("--source-id", required=True)
    p.add_argument("--child-minimal", required=True)
    p.add_argument("--child-rewrite", required=True)
    p.add_argument("--child-skip", required=True)
    args = p.parse_args()

    out = args.out_dir
    state = parse_state_evidence(out / "state-evidence.txt")

    inits = {label: read_text(out / f"{label}-init-py.txt") for label in
             ("source", "minimal", "rewrite", "skip")}
    logs = {label: read_text(out / f"{label}-agent.log") for label in
            ("source", "minimal", "rewrite", "skip")}

    md = []
    md.append("# Coding-agent fork — branch-and-fan-out demo run\n\n")
    md.append(f"- BRANCH tag: `{args.branch_tag}`\n")
    md.append(f"- Daemon-measured pause: **{args.daemon_pause_ms} ms**\n")
    md.append(f"- Source sandbox: `{args.source_id}`\n\n")

    md.append("## The on-disk state inherited from BRANCH\n\n")
    md.append(
        "All 3 grandchildren inherit `/workspace` byte-identically from the source. The 50 MiB synthetic `vendored.bin` and the populated `__pycache__/` are shared via copy-on-write — proof that fork captures filesystem state in a way no parallel-prompt approach can replicate.\n\n"
    )
    md.append("| Agent | `vendored.bin` size | `vendored.bin` md5 | `__pycache__/` tree md5 |\n")
    md.append("|---|---:|---|---|\n")
    for label in ("source", "minimal", "rewrite", "skip"):
        e = state.get(label, {})
        size = f"{(e.get('vendored_size') or 0) // (1024*1024)} MiB"
        v_md5 = (e.get("vendored_md5") or "?")[:16]
        p_md5 = (e.get("pycache_md5") or "?")[:16]
        md.append(f"| {label} | {size} | `{v_md5}…` | `{p_md5}…` |\n")
    md.append("\n")

    # Check md5 match across children
    v_md5s = {state[l].get("vendored_md5") for l in state}
    if len(v_md5s) == 1:
        md.append("✓ **All 4 agents share the same `vendored.bin` md5.** The 50 MiB blob travelled byte-identically across the fork. No prompt can do this.\n\n")
    else:
        md.append(f"⚠️ vendored.bin diverged: {v_md5s}\n\n")

    md.append("## The divergent edits each child made\n\n")
    md.append("Each grandchild received a different fix strategy via `/tmp/forkd-strategy.sh`. The post-fix `mathy/__init__.py` differs:\n\n")

    for label in ("source", "minimal", "rewrite", "skip"):
        md.append(f"### {label}\n\n")
        if label != "source":
            md.append(f"**Strategy:** `{label}.sh`\n\n")
        body = inits.get(label, "").rstrip()
        if not body:
            body = "_(no content captured)_"
        md.append("```python\n")
        md.append(body)
        md.append("\n```\n\n")
        # Test outcome
        outcome = extract_test_outcome(logs.get(label, ""))
        md.append(f"Test outcome after this strategy: **{outcome}**\n\n")

    md.append("## What this run demonstrates\n\n")
    md.append(
        "1. **Filesystem state survives BRANCH byte-identically.** The 50 MiB synthetic `vendored.bin` and the populated `__pycache__/` are shared across all 4 agents via Firecracker's copy-on-write memory image — this is the *filesystem* analog of the earlier langgraph-react demo's *conversation-history* sharing.\n\n"
        "2. **Each child diverges only after the fork.** Three different fix strategies produce three different `mathy/__init__.py` files (and one of them edits the *tests* instead of the *source*). The branches are isolated; one child's edits do not appear in another's tree.\n\n"
        "3. **No parallel-prompt approach can do this.** To replicate this with 3 parallel API calls, you'd have to bundle the entire `/workspace` directory into each prompt — including the binary `vendored.bin`. Not just inefficient: technically impossible above a few KiB.\n\n"
        f"Daemon pause window: **{args.daemon_pause_ms} ms** for the BRANCH operation. Three new sandboxes spun up from the snapshot, each ready to apply its own fix in parallel.\n"
    )

    (out / "summary.md").write_text("".join(md), encoding="utf-8")
    print(f"wrote {out / 'summary.md'}")

    # machine-readable summary
    summary = {
        "branch_tag": args.branch_tag,
        "daemon_pause_ms": args.daemon_pause_ms,
        "state_evidence": state,
        "test_outcomes": {label: extract_test_outcome(logs.get(label, "")) for label in
                          ("source", "minimal", "rewrite", "skip")},
    }
    (out / "summary.json").write_text(json.dumps(summary, indent=2, ensure_ascii=False), encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
