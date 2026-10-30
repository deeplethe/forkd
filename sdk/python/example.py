"""End-to-end example using the sandstorm Python SDK (E2B-compatible).

Requires a master, reached through SANDSTORM_URL, SANDSTORM_TOKEN and
SANDSTORM_CA, with a template named `python` (override with the
SANDSTORM_TEMPLATE env var, or set SANDSTORM_TAG to fork a snapshot).
"""

import os
import time

from sandstorm import Sandbox

if not os.environ.get("SANDSTORM_TEMPLATE") and not os.environ.get("SANDSTORM_TAG"):
    os.environ["SANDSTORM_TEMPLATE"] = "python"

print("=== with-block (auto-kill on exit) ===")
with Sandbox() as sandbox:
    print("sandbox:", sandbox.id, sandbox.ping())

    t = time.perf_counter()
    r = sandbox.commands.run("echo hello from sandbox")
    print(f"exec echo [{(time.perf_counter()-t)*1000:.0f} ms]: {r.stdout.rstrip()}")

    t = time.perf_counter()
    result = sandbox.eval("print(sum(range(5)))")
    print(f"eval [{(time.perf_counter()-t)*1000:.0f} ms]: {result.rstrip()}")

print()
print("=== manual lifecycle ===")
sandbox = Sandbox.create()
print(sandbox.commands.run("uname -a").stdout.rstrip())
sandbox.kill()
print("killed.")
