#!/usr/bin/env bash
# Strategy: minimal — one-line sed patch to fix the bug.
# Inherits the full /workspace from BRANCH; only modifies __init__.py.
set -e
cd /workspace
echo "=== strategy: minimal (one-line sed) ===" >> .agent-log
sed -i 's/return a - b/return a + b/' mathy/__init__.py
echo "=== mathy/__init__.py after patch ===" >> .agent-log
cat mathy/__init__.py >> .agent-log
echo "=== re-running tests ===" >> .agent-log
python3 -m unittest tests.test_add -v 2>>.agent-log
echo "=== final test exit code: $? ===" >> .agent-log
