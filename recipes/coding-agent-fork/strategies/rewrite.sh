#!/usr/bin/env bash
# Strategy: rewrite — replace the function with a more defensive
# implementation, also handling type coercion / overflow concerns.
# Same /tmp/workspace state inherited as the other strategies.
set -e
cd /tmp/workspace
echo "=== strategy: rewrite (full function replacement) ===" >> .agent-log
cat > mathy/__init__.py <<'PY'
"""Tiny math helpers. Imported by tests/test_add.py.

Rewritten by the 'rewrite' branch with explicit type handling.
"""


def add(a, b):
    # Be explicit about the int contract; defensive against the
    # subtle bug a previous version had (it returned a - b).
    if not isinstance(a, (int, float)) or not isinstance(b, (int, float)):
        raise TypeError(f"add expects numeric inputs, got {type(a)} and {type(b)}")
    return a + b


def double(x):
    return x * 2
PY
echo "=== mathy/__init__.py after rewrite ===" >> .agent-log
cat mathy/__init__.py >> .agent-log
echo "=== re-running tests ===" >> .agent-log
python3 -m unittest tests.test_add -v 2>>.agent-log
echo "=== final test exit code: $? ===" >> .agent-log
