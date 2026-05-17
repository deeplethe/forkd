#!/usr/bin/env python3
"""
ReAct agent for the forkd branch-and-fan-out demo.

Why this is not actually using LangGraph: the demo's value is in
forkd's BRANCH primitive, not in the agent framework. LangChain
adds 60 MiB of memory + 4 GiB of pip wheels and zero clarity for
this task. We implement the loop directly against an
OpenAI-compatible chat completions endpoint (SiliconFlow). If you
need to port to LangGraph for production, the chat-completions call
maps 1:1 to a langgraph node.

The agent:

- Runs a ReAct loop solving a "plan a trip" task
- Reads `/tmp/forkd-hint.txt` BEFORE every LLM call. If the file
  is non-empty, its contents are prepended to the next user
  message as a steering instruction. This is the side-channel
  the orchestrator uses to make grandchildren diverge after
  branching.
- Writes one JSONL line per step to stdout (so the orchestrator
  can capture transcripts via `forkd-controller exec`).
- After a configurable number of steps, prints a "READY_TO_BRANCH"
  marker line and pauses for `--branch-wait-s` seconds — the
  orchestrator polls for this marker, then triggers BRANCH.
- Resumes its loop after the branch wait, picking up the hint
  the orchestrator (may have) written.

JSONL schema (one event per line):

    {"event":"start", "task": str, "t_ms": int}
    {"event":"think", "step": int, "content": str, "t_ms": int}
    {"event":"tool_call", "step": int, "name": str, "args": dict, "t_ms": int}
    {"event":"tool_result", "step": int, "name": str, "result": str, "t_ms": int}
    {"event":"hint", "step": int, "hint": str, "t_ms": int}
    {"event":"answer", "step": int, "content": str, "t_ms": int}
    {"event":"ready_to_branch", "t_ms": int}
    {"event":"resumed", "t_ms": int}
    {"event":"stop", "steps": int, "total_tokens": int, "wall_ms": int}
"""
import argparse
import json
import os
import sys
import time
from pathlib import Path
from typing import Any

import requests

# ``tools`` is a sibling file in the same directory inside the rootfs.
# At install time we drop both files into /opt/forkd-demo/.
sys.path.insert(0, str(Path(__file__).resolve().parent))
from tools import TOOL_FNS, TOOLS_SPEC  # noqa: E402


HINT_FILE = Path("/tmp/forkd-hint.txt")
LOG_FILE = Path("/tmp/forkd-agent-stdout.log")


def emit(obj: dict) -> None:
    """Append one JSONL event.

    We write to BOTH the in-rootfs log file and stdout so that
    (a) the daemon's `exec` captures it while the agent is the
    foreground process, and (b) after BRANCH each grandchild has
    its own copy-on-write log that the orchestrator can `cat`.
    Children's stdout points back to the SOURCE's exec pipe which
    becomes dead post-fork, so file-based collection is what
    actually survives the fan-out.
    """
    obj.setdefault("t_ms", now_ms())
    line = json.dumps(obj, ensure_ascii=False, separators=(",", ":")) + "\n"
    try:
        with LOG_FILE.open("a", encoding="utf-8") as f:
            f.write(line)
    except OSError:
        pass
    try:
        sys.stdout.write(line)
        sys.stdout.flush()
    except OSError:
        pass


def now_ms() -> int:
    return int(time.time() * 1000)


def read_hint() -> str:
    """Read /tmp/forkd-hint.txt. Empty string if missing.

    The hint is appended to the running conversation as a system-
    level steering message before each LLM call. The agent does NOT
    erase its own thoughts on hint change; the hint just shapes the
    next decision.
    """
    try:
        return HINT_FILE.read_text(encoding="utf-8").strip()
    except FileNotFoundError:
        return ""


def chat_completion(
    *,
    base_url: str,
    api_key: str,
    model: str,
    messages: list,
    tools: list,
    temperature: float = 0.3,
    timeout_s: int = 60,
) -> dict:
    """Single chat-completion call. Returns the raw choices[0].message dict."""
    resp = requests.post(
        f"{base_url.rstrip('/')}/chat/completions",
        headers={
            "Authorization": f"Bearer {api_key}",
            "Content-Type": "application/json",
        },
        json={
            "model": model,
            "messages": messages,
            "tools": tools,
            "temperature": temperature,
        },
        timeout=timeout_s,
    )
    resp.raise_for_status()
    return resp.json()


def run_step(
    *,
    step: int,
    messages: list,
    base_url: str,
    api_key: str,
    model: str,
    temperature: float,
) -> tuple[bool, int]:
    """One ReAct step. Returns (done, tokens_used).

    Reads the hint just before the LLM call. If a hint is present
    we append it as a system message at the END of the conversation
    (recent steering wins over earlier system prompt).
    """
    hint = read_hint()
    if hint:
        messages = messages + [
            {"role": "system", "content": f"Updated steering hint: {hint}"}
        ]
        emit({"event": "hint", "step": step, "hint": hint})

    resp = chat_completion(
        base_url=base_url,
        api_key=api_key,
        model=model,
        messages=messages,
        tools=TOOLS_SPEC,
        temperature=temperature,
    )
    msg = resp["choices"][0]["message"]
    tokens = resp.get("usage", {}).get("total_tokens", 0)

    # Always log the model's "thought" (its content field, if any)
    if msg.get("content"):
        emit({"event": "think", "step": step, "content": msg["content"]})

    tool_calls = msg.get("tool_calls") or []

    # Add the assistant message to history. We strip the hint we
    # appended for this call so it doesn't pollute the persisted
    # conversation — only the LLM's response is permanent.
    messages.append(msg)

    if not tool_calls:
        # Model didn't request a tool → treat as final answer.
        emit({"event": "answer", "step": step, "content": msg.get("content", "")})
        return True, tokens

    # Execute each tool call and append a tool message.
    for tc in tool_calls:
        name = tc["function"]["name"]
        args = json.loads(tc["function"]["arguments"] or "{}")
        emit({"event": "tool_call", "step": step, "name": name, "args": args})
        fn = TOOL_FNS.get(name)
        if fn is None:
            result = f"unknown tool: {name}"
        else:
            try:
                result = fn(**args)
            except Exception as e:
                result = f"tool error: {e}"
        emit({"event": "tool_result", "step": step, "name": name, "result": result})
        messages.append(
            {
                "role": "tool",
                "tool_call_id": tc["id"],
                "content": json.dumps(result, ensure_ascii=False),
            }
        )

    return False, tokens


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__.strip().splitlines()[0])
    p.add_argument(
        "--task",
        default="Plan a 2-day trip to Kyoto and Osaka. Use the tools to check weather and find places. Output a concrete day-by-day itinerary at the end.",
    )
    p.add_argument(
        "--branch-after-step",
        type=int,
        default=3,
        help="After this many steps, emit READY_TO_BRANCH and pause for --branch-wait-s",
    )
    p.add_argument(
        "--branch-wait-s",
        type=int,
        default=30,
        help="Seconds to sleep at the branch point so the orchestrator can fork (default 30)",
    )
    p.add_argument("--max-steps", type=int, default=10)
    p.add_argument(
        "--base-url",
        default=os.environ.get("LLM_BASE_URL", "https://api.siliconflow.cn/v1"),
    )
    p.add_argument("--model", default=os.environ.get("LLM_MODEL", "Qwen/Qwen2.5-7B-Instruct"))
    p.add_argument("--temperature", type=float, default=0.4)
    p.add_argument(
        "--api-key",
        default=os.environ.get("LLM_API_KEY") or os.environ.get("SILICONFLOW_API_KEY"),
        help="LLM API key; falls back to LLM_API_KEY or SILICONFLOW_API_KEY env",
    )
    args = p.parse_args()

    if not args.api_key:
        emit({"event": "error", "what": "no API key (set LLM_API_KEY or SILICONFLOW_API_KEY)"})
        return 2

    t0 = now_ms()
    emit({"event": "start", "task": args.task, "model": args.model})

    system = (
        "You are a careful trip-planning agent. Use the `weather` and "
        "`search_places` tools to gather facts BEFORE proposing an "
        "itinerary. Think one step at a time. When you have enough "
        "information, produce a concrete day-by-day plan and stop."
    )
    messages: list[dict[str, Any]] = [
        {"role": "system", "content": system},
        {"role": "user", "content": args.task},
    ]

    total_tokens = 0
    step = 0
    while step < args.max_steps:
        step += 1
        done, tokens = run_step(
            step=step,
            messages=messages,
            base_url=args.base_url,
            api_key=args.api_key,
            model=args.model,
            temperature=args.temperature,
        )
        total_tokens += tokens
        if done:
            break

        if step == args.branch_after_step:
            emit({"event": "ready_to_branch"})
            # Hold for the orchestrator. The whole VM may get paused
            # by BRANCH partway through this sleep — that's fine,
            # we just wake up later when the wall clock catches up.
            time.sleep(args.branch_wait_s)
            emit({"event": "resumed"})

    emit(
        {
            "event": "stop",
            "steps": step,
            "total_tokens": total_tokens,
            "wall_ms": now_ms() - t0,
        }
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
