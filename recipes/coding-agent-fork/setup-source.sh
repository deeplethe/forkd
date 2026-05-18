#!/usr/bin/env bash
# Runs inside the source sandbox BEFORE branch. Sets up a tiny
# buggy Python package + runs the failing test once so the
# `.pytest_cache`-equivalent (Python's __pycache__) is populated
# in /workspace before BRANCH captures the state.

set -e

export TERM=${TERM:-xterm-256color}
date -s "@$1" >/dev/null 2>&1 || true   # arg 1 = host_now from orchestrator

mkdir -p /workspace
cd /workspace

# Wipe any prior state — the snapshot is shared with langgraph-react.
rm -rf mathy tests __pycache__ build-artifacts .agent-log

# --- The "project" --------------------------------------------------
mkdir -p mathy tests build-artifacts

cat > mathy/__init__.py <<'PY'
"""Tiny math helpers. Imported by tests/test_add.py."""

def add(a, b):
    # BUG: should be a + b. Three forked agents will fix this in
    # three different ways below.
    return a - b


def double(x):
    return x * 2
PY

cat > tests/test_add.py <<'PY'
import sys
sys.path.insert(0, '/workspace')

import unittest
from mathy import add, double


class AddTests(unittest.TestCase):
    def test_add_positive(self):
        self.assertEqual(add(2, 3), 5)

    def test_add_zero(self):
        self.assertEqual(add(0, 0), 0)

    def test_add_negative(self):
        self.assertEqual(add(-1, 1), 0)

    def test_double(self):
        self.assertEqual(double(5), 10)


if __name__ == "__main__":
    unittest.main()
PY

# --- Simulate "expensive build artifacts" ----------------------------
# 50 MB of synthetic data representing things a real coding agent
# would have accumulated: vendored dependencies, downloaded datasets,
# compiled extensions, pre-trained model weights. The point isn't
# what the data is — it's that this state is shared by all 3 forked
# children byte-identically, and no parallel prompt could carry it.
dd if=/dev/urandom of=build-artifacts/vendored.bin bs=1M count=50 status=none

# --- Run the failing test once, populating __pycache__ ---------------
echo "=== initial test run (should fail at add) ===" > /workspace/.agent-log
python3 -m unittest tests.test_add -v 2>>/workspace/.agent-log || true

# Record the byte-identical baseline files
echo "=== baseline /workspace contents at branch point ===" >> /workspace/.agent-log
find /workspace -type f | sort >> /workspace/.agent-log
echo "" >> /workspace/.agent-log
echo "=== baseline mathy/__init__.py ===" >> /workspace/.agent-log
cat /workspace/mathy/__init__.py >> /workspace/.agent-log

echo "source: setup complete; entering wait for BRANCH"
echo "ready_to_branch"  # marker the orchestrator polls for

# Park for the orchestrator. 60s is generous — orchestrator typically
# branches within 5s of seeing the marker.
sleep 60

echo "source: post-branch wake (this only fires in the source, not children)"
