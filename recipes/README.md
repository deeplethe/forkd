# Recipes

Ready-made parent-rootfs recipes for common workbench images.
Each recipe takes a public Docker / OCI image and turns it into a
forkd parent snapshot, so you can fork N warmed children from it
in milliseconds.

The pattern is the same across recipes:

```bash
# 1. Build a parent rootfs from an upstream image
sudo bash recipes/<name>/build.sh

# 2. Snapshot the warmed parent (one-time per image version)
sudo forkd snapshot --tag <name> \
    --kernel ./vmlinux-6.1.141 \
    --rootfs recipes/<name>/parent.ext4 \
    --tap forkd-tap0

# 3. Fork N children, fan-out workload
sudo -E forkd fork --tag <name> -n 100 --per-child-netns
```

## Available recipes

| Recipe | Parent image | Size | Audience |
|---|---|---|---|
| [`python-numpy/`](./python-numpy/) | `python:3.12-slim` + `python3-numpy` | ~1.5 GB | The canonical fan-out demo; what the chart on the front README measures |
| [`e2b-codeinterpreter/`](./e2b-codeinterpreter/) | `e2bdev/code-interpreter` | ~600 MB | AI code-execution agents (Anthropic / OpenAI tutorials use this image). Lightest "agent ready" option |
| [`jupyter-kernel/`](./jupyter-kernel/) | `quay.io/jupyter/scipy-notebook` | ~3 GB | Code-interpreter / notebook-style agents — full SciPy stack pre-imported, ~1 ms per fresh kernel instead of ~2 s |
| [`coding-agent/`](./coding-agent/) | `python:3.12` + git + ruff + black + pytest | ~1.8 GB | SWE-style coding agents that need a real dev toolchain inside the sandbox |
| [`nodejs/`](./nodejs/) | `node:22-slim` | ~250 MB | JavaScript / TypeScript workloads (Jest, Playwright fan-out) |
| [`agent-workbench/`](./agent-workbench/) | `agent-infra/sandbox` (browser + VSCode + Jupyter + MCP + shell) | ~5 GB | Kitchen-sink agent workbench when you want every tool already mounted; trades a bigger memory.bin for batteries-included |

## Choosing a recipe

- **You're benchmarking** → `python-numpy/`
- **You're running an AI code interpreter** → `e2b-codeinterpreter/`
- **You need the full SciPy / notebook stack** → `jupyter-kernel/`
- **You're running a coding agent (SWE-bench style)** → `coding-agent/`
- **JS / TS only** → `nodejs/`
- **You want browser + IDE + everything in one box** → `agent-workbench/`

## Notes

- Recipes are tested on Ubuntu 24.04 / Linux 6.14 / x86_64. Other distros
  may need adjustments to `scripts/build-rootfs.sh`.
- The first-time `build.sh` of each recipe takes a few minutes (pulling
  the Docker image + converting to ext4). The snapshot step is ~10 s.
  After that, forking children is the published benchmark cost.
- Each recipe is self-contained — pick one, run it; you don't need to
  understand the others.
