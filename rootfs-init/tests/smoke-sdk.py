"""Local smoke test for the SDK's result_json deserialization path.

Exercises Sandbox.eval()'s new branch without actually talking to a
guest. Run on the host with:  python3 rootfs-init/smoke-sdk.py
"""
import json
import sys
import os

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "sdk", "python"))
from forkd.sandbox import Sandbox


class _StubSandbox(Sandbox):
    """Sandbox subclass that skips _spawn() and stubs _send for tests."""

    def __init__(self, reply: dict) -> None:
        self._reply = reply
        # Skip spawning a real microVM.
        Sandbox.__init__(self, spawn=False)

    def _send(self, _msg):
        return self._reply


# Node-recipe path: result_json present.
sb1 = _StubSandbox({"result_json": json.dumps("Example Domain"), "exit_code": 0})
out1 = sb1.eval("await page.title()")
assert out1 == "Example Domain", f"node path: got {out1!r}"
print(f"node recipe path OK: {out1!r}")

# Node-recipe path with structured result.
sb2 = _StubSandbox({"result_json": json.dumps({"a": 1, "b": [2, 3]}), "exit_code": 0})
out2 = sb2.eval("...")
assert out2 == {"a": 1, "b": [2, 3]}, f"structured: got {out2!r}"
print(f"node recipe structured OK: {out2!r}")

# Python-recipe path: only `result` (repr string), backwards-compat.
sb3 = _StubSandbox({"result": "[0.0, 0.0, 0.0]", "exit_code": 0})
out3 = sb3.eval("numpy.zeros(3).tolist()")
assert out3 == "[0.0, 0.0, 0.0]", f"python path: got {out3!r}"
print(f"python recipe path OK: {out3!r}")

# Error path.
sb4 = _StubSandbox({"error": "ReferenceError: foo is not defined", "exit_code": 1})
try:
    sb4.eval("foo()")
except RuntimeError as e:
    print(f"error path OK: {e}")
else:
    raise AssertionError("expected RuntimeError")

print("all SDK paths OK")
