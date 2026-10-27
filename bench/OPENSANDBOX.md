# OpenSandbox bench methodology

## Result

OpenSandbox N=100 spawn measured at **121,958 ms** on the same dev box
sandstorm was measured on (Ubuntu 24.04 / Linux 6.14 / 20 vCPU / 30 GiB /
KVM). 0 of 100 sandboxes succeeded the workload (`import numpy`):
some failed at the bootstrap step (`execd` archive copy timed out),
the rest finished spawn but the in-container `python3 -c "import
numpy"` returned non-zero because `python:3.12-slim` lacks numpy —
the same condition Docker, gVisor, and BoxLite face.

## Setup

```bash
# uvx (uv) is the recommended runner — installs in an isolated env.
curl -LsSf https://astral.sh/uv/install.sh | sh
~/.local/bin/uvx opensandbox-server init-config ~/.opensandbox.toml --example docker
# Edit ~/.opensandbox.toml: server.port = 18080 (8080 was occupied on this host).
python3.13 -m pip install --user opensandbox==0.1.8

# Then run the server (insecure mode for the bench — no token auth):
OPENSANDBOX_INSECURE_SERVER=YES nohup ~/.local/bin/uvx opensandbox-server \
    --config ~/.opensandbox.toml > /tmp/opensandbox-server.log 2>&1 &
```

Note: the official `opensandbox/code-interpreter:v1.0.2` image was
**not pullable** during this measurement run — the registry returned
"could not fetch content descriptor sha256:cc80778f..." for one of
the layers. The bench falls back to `python:3.12-slim` which is
already on the host.

## Workload

`bench/opensandbox-bench.py` (wired into [`compare-all.py`](./compare-all.py))
issues N concurrent `Sandbox.create(...)` calls against the local
server.

```python
from opensandbox import Sandbox
from opensandbox.config.connection import ConnectionConfig

CONN = ConnectionConfig(
    domain="127.0.0.1:18080", protocol="http",
    request_timeout=timedelta(seconds=120),
)

async def one(_i):
    sandbox = await Sandbox.create(
        "python:3.12-slim",
        entrypoint=["sleep", "60"],
        timeout=timedelta(minutes=2),
        ready_timeout=timedelta(seconds=60),
        connection_config=CONN,
    )
    try:
        exe = await sandbox.commands.run(
            "python3 -c 'import numpy; print(numpy.zeros(5).tolist())'"
        )
        return getattr(exe, "exit_code", 0) == 0
    finally:
        await sandbox.kill()
```

## Notes

OpenSandbox is an **abstraction layer** over multiple runtimes
(Docker, K8s, gVisor, Kata, Firecracker). On this host we tested
the Docker runtime (the default in `--example docker`). The
container-create + execd-copy + start path takes ~4-5 s per sandbox
serially; N=100 in parallel hits the host's docker daemon's
concurrency ceiling and a substantial number of requests time out
(`httpx.ReadTimeout` in the SDK).

A K8s-backed deployment of OpenSandbox would likely produce a
different number — that's the design point of the abstraction layer.
Whatever runtime you point it at, OpenSandbox itself adds the
abstraction-layer cost; it cannot be faster than its underlying
runtime on this workload.
