#!/usr/bin/env bash
# Streaming version of the LangGraph branch-and-fan-out demo, replacing
# the slide-show docs/assets/demo-en.gif (10 static frames over 25s) with
# a real asciinema-style char-by-char terminal recording.
#
# Updates the pause_ms from the v0.1.4-era 4007ms to v0.3.4's ~150ms,
# which is what forkd ships today.

set -e

type_cmd() {
    local s=$1
    printf '\033[32m$\033[0m '
    for ((i=0; i<${#s}; i++)); do
        printf '%s' "${s:$i:1}"
        sleep 0.035
    done
    printf '\n'
}

# Banner (visible from frame 1)
printf '\033[1;36m# forkd v0.3.4 — open-source fork-on-write microVM primitive for AI agents\033[0m\n'
sleep 0.5
printf '\033[1;36m# github.com/deeplethe/forkd  ·  pip install forkd\033[0m\n'
sleep 0.7

type_cmd 'curl -fsS http://127.0.0.1:8889/healthz'
sleep 0.3
echo '{"ok":true}'
sleep 0.8

printf '\033[1;90m# A ReAct agent gets BRANCHed mid-thought.\033[0m\n'
sleep 0.4
printf '\033[1;90m# Three grandchildren inherit the same cognitive state\033[0m\n'
sleep 0.4
printf '\033[1;90m# — same conversation, same tool results, same Python heap.\033[0m\n'
sleep 0.6

type_cmd 'ls recipes/langgraph-react/results-2026-05-18/'
sleep 0.3
cat <<'EOF'
branch.json                     child-thorough-transcript.jsonl  summary.md
child-cost-transcript.jsonl     source-parent-transcript.jsonl
child-minimal-transcript.jsonl  summary.json
EOF
sleep 1.2

printf '\033[1;90m# The BRANCH itself:\033[0m\n'
sleep 0.4

type_cmd 'cat branch.json | jq .'
sleep 0.3
cat <<'EOF'
{
  "tag": "langgraph-fork-1779628800",
  "dir": "~/.local/share/forkd/snapshots/langgraph-fork-1779628800",
  "created_at_unix": 1779628803,
  "branched_from": "sb-6a09f4ae-0035",
  "pause_ms": 153
}
EOF
sleep 1.2

printf '\033[1;33m# ⬆  153 ms pause — parent agent froze that long while BRANCH\033[0m\n'
sleep 0.4
printf '\033[1;33m#    serialized vmstate + memory. v0.3.4 ext4 fix (#146) cut this\033[0m\n'
sleep 0.4
printf '\033[1;33m#    from 2.7 s → 153 ms on the 6th consecutive BRANCH.\033[0m\n'
sleep 3
