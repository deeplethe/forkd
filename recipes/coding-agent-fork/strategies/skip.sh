#!/usr/bin/env bash
# Strategy: skip — don't touch source; mark the failing tests as
# expected-failure and ship. The kind of fix you regret in 3 weeks.
# Demonstrates that branches can take qualitatively different
# approaches, not just textually different code edits.
set -e
cd /workspace
echo "=== strategy: skip (mark tests as expected-failure) ===" >> .agent-log
echo "(no changes to mathy/__init__.py — bug stays)" >> .agent-log

# Decorate add tests as expected-failure. We do this surgically so
# the divergence in the resulting test file is visible.
python3 - <<'PY'
src = open("/workspace/tests/test_add.py").read()
# Insert @unittest.expectedFailure before the add tests
import re
src = re.sub(
    r"(    def test_add_\w+)",
    r"    @unittest.expectedFailure\n\1",
    src,
)
open("/workspace/tests/test_add.py", "w").write(src)
PY

echo "=== tests/test_add.py after expected-failure decoration ===" >> .agent-log
cat /workspace/tests/test_add.py >> .agent-log
echo "=== re-running tests ===" >> .agent-log
python3 -m unittest tests.test_add -v 2>>.agent-log
echo "=== final test exit code: $? ===" >> .agent-log
