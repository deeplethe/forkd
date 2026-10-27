"""End-to-end example using the sandstorm Python SDK (E2B-compatible).

Requires:
  - The `sandstorm` Rust CLI on PATH.
  - A parent snapshot named `pyagent` (override with SANDSTORM_TAG env var).
  - The host-tap.sh tap up so the guest is reachable at 10.42.0.2:8888.
"""

import time

from sandstorm import Sandbox

print("=== with-block (auto-kill on exit) ===")
with Sandbox() as sandbox:
    print("agent:", sandbox.ping())

    t = time.perf_counter()
    r = sandbox.commands.run("python3 -c 'import numpy; print(numpy.eye(3))'")
    print(f"exec (fresh subprocess) [{(time.perf_counter()-t)*1000:.0f} ms]")
    print(r.stdout.rstrip())

    t = time.perf_counter()
    r = sandbox.commands.run("echo hello from sandbox")
    print(f"exec echo [{(time.perf_counter()-t)*1000:.0f} ms]: {r.stdout.rstrip()}")

    t = time.perf_counter()
    result = sandbox.eval("numpy.zeros(5).tolist()")
    print(f"eval (warm PID-1 numpy) [{(time.perf_counter()-t)*1000:.0f} ms]: {result}")

print()
print("=== manual lifecycle ===")
sandbox = Sandbox.create()
print(sandbox.commands.run("uname -a").stdout.rstrip())
sandbox.kill()
print("killed.")
