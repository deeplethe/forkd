# forkd — Python SDK

E2B-compatible sandbox API backed by [forkd](https://github.com/deeplethe/forkd).

```python
from forkd import Sandbox

with Sandbox() as sandbox:
    result = sandbox.commands.run("python3 -c 'import numpy; print(numpy.zeros(3))'")
    print(result.stdout)        # [0. 0. 0.]
    print(result.exit_code)     # 0
```

Or with explicit lifecycle:

```python
sandbox = Sandbox()
result = sandbox.commands.run("echo hello")
sandbox.kill()
```

## Bonus: warmed-state eval

If your snapshot parent imported numpy, you can skip subprocess overhead
and use the warmed PID-1 interpreter directly:

```python
with Sandbox() as sandbox:
    out = sandbox.eval("numpy.zeros(5).tolist()")   # ~8 ms
    # vs commands.run("python3 -c '...'") which is ~108 ms (fresh subprocess)
```

## Requirements

The `forkd` Rust CLI must be installed and on `PATH`, plus a parent snapshot
must already exist (`forkd snapshot --tag pyagent ...`). See the main
[README](https://github.com/deeplethe/forkd) for the full setup.

## Status

Pre-alpha. Currently supports:
- `Sandbox()` / `Sandbox.create()` — spawn one sandbox
- `sandbox.commands.run(cmd)` — run command, get stdout/stderr/exit_code
- `sandbox.eval(expr)` — eval Python in warmed PID 1
- `sandbox.kill()` — terminate

Not yet (blocked on issues #1 + #4):
- Multiple concurrent `Sandbox()` instances (all share parent's MAC/IP)
- `sandbox.files.read/write` — filesystem operations
- Streaming output
