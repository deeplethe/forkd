#!/usr/bin/env bash
# Smoke-test forkd-agent's recipe bridge against a fake warmup process.
# Run on the dev box: copy this + forkd-agent.py + fake-warmup.py to
# /tmp/, then `bash /tmp/smoke-test.sh`.
set -euo pipefail

cd /tmp
pkill -f forkd-agent.py 2>/dev/null || true
sleep 0.5

export FORKD_WARMUP_CMD="python3 /tmp/fake-warmup.py"
export FORKD_AGENT_LANG="node"

# Pick a free port. The agent listens on 8888 by default — we can't
# change that without code edit, so just check the port is free.
if ss -ltn | awk '{print $4}' | grep -q ':8888$'; then
    echo "port 8888 busy; aborting" >&2
    exit 1
fi

nohup python3 /tmp/forkd-agent.py > /tmp/forkd-agent.log 2>&1 &
AGENT_PID=$!
echo "agent pid=$AGENT_PID"

# Wait for "agent listening" line.
for _ in $(seq 1 30); do
    if grep -q 'agent listening' /tmp/forkd-agent.log 2>/dev/null; then
        break
    fi
    sleep 0.2
done

echo "=== agent log ==="
cat /tmp/forkd-agent.log
echo "================="

# Ping
echo
echo "[ping]"
printf '%s\n' '{"action":"ping"}' | nc -q1 127.0.0.1 8888

# Eval through bridge
echo
echo "[eval via bridge]"
printf '%s\n' '{"action":"eval","code":"await page.goto(\"https://example.com\")"}' | nc -q1 127.0.0.1 8888

# Tear down
kill $AGENT_PID 2>/dev/null || true
echo
echo "[done]"
