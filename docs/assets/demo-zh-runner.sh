#!/usr/bin/env bash
# 中文版 streaming langgraph-react demo.
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

printf '\033[1;36m# forkd v0.3.4 — 给 AI Agent 用的开源微 VM fork-on-write 原语\033[0m\n'
sleep 0.5
printf '\033[1;36m# github.com/deeplethe/forkd  ·  pip install forkd\033[0m\n'
sleep 0.7

type_cmd 'curl -fsS http://127.0.0.1:8889/healthz'
sleep 0.3
echo '{"ok":true}'
sleep 0.8

printf '\033[1;90m# ReAct agent 在 LangGraph 中跑到一半被 BRANCH。\033[0m\n'
sleep 0.4
printf '\033[1;90m# 3 个子沙箱继承同一份认知状态 —— 同对话、同工具结果、同 Python 堆。\033[0m\n'
sleep 0.6

type_cmd 'ls recipes/langgraph-react/results-2026-05-18/'
sleep 0.3
cat <<'EOF'
branch.json                     child-thorough-transcript.jsonl  summary.md
child-cost-transcript.jsonl     source-parent-transcript.jsonl
child-minimal-transcript.jsonl  summary.json
EOF
sleep 1.2

printf '\033[1;90m# 看 BRANCH 本身:\033[0m\n'
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

printf '\033[1;33m# ⬆  153 ms 卡顿 — BRANCH 序列化 vmstate + 内存的时间\033[0m\n'
sleep 0.4
printf '\033[1;33m#    v0.3.4 的 ext4 修复 (#146) 把第 6 次连续 BRANCH 从\033[0m\n'
sleep 0.4
printf '\033[1;33m#    2.7 秒砍到 153 毫秒。\033[0m\n'
sleep 3
