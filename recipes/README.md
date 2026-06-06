# Recipes

forkd is a fork-on-write microVM primitive — the "for AI agents"
framing on the front page is one prominent use case, not the
ceiling. Anything that wants **N isolated children spawned from a
warmed parent in milliseconds** fits.

## Pick your starting point

### By problem you're solving

| Problem | Recipes | What forkd buys you |
|---|---|---|
| **AI agent fan-out** — try N approaches, branch a thinking agent | [`langgraph-react/`](./langgraph-react/) · [`crewai-fanout/`](./crewai-fanout/) · [`autogen-branch/`](./autogen-branch/) · [`openai-swarm/`](./openai-swarm/) · [`mcp-agent/`](./mcp-agent/) · [`speculative-agent/`](./speculative-agent/) · [`coding-agent-fork/`](./coding-agent-fork/) | Per-child KVM isolation + warmed runtime inheritance. The "fork mid-thought" story. |
| **CI test parallelism** — run 100 pytest workers from a warmed parent | [`postgres-fixture/`](./postgres-fixture/) (DB-per-test) · [`ci-parallel-pytest/`](./ci-parallel-pytest/) (worker fan-out) | Skip per-worker container cold-start + dependency install. ~50 ms / worker instead of ~3 s. |
| **Database test fixtures** — fresh, isolated postgres per test | [`postgres-fixture/`](./postgres-fixture/) | `initdb` runs **once** at parent build; every fork inherits the post-init state. ~200× faster than per-test container. |
| **Browser automation farms** — Playwright / Puppeteer fan-out at scale | [`playwright-browser/`](./playwright-browser/) | Fork warmed headless Chromium at ~10 ms instead of ~2 s cold-boot. |
| **Notebook / code interpreter** — Jupyter kernel per session | [`jupyter-kernel/`](./jupyter-kernel/) · [`e2b-codeinterpreter/`](./e2b-codeinterpreter/) | Full SciPy stack pre-imported. ~1 ms per fresh kernel. |
| **General-purpose compute fan-out** — anything that needs N warmed sandboxes | [`python-numpy/`](./python-numpy/) · [`coding-agent/`](./coding-agent/) · [`nodejs/`](./nodejs/) · [`agent-workbench/`](./agent-workbench/) | Pre-baked language runtime + canonical fan-out benchmark. |

### By integration framework (host-side Python scripts)

If you're plugging forkd into an existing agent framework, these
are ~150-250 lines of Python with a `--dry-run` mode so you can
verify the forkd plumbing without an LLM key.

| Framework | Recipe |
|---|---|
| Claude Desktop / Cursor / Cline (via MCP) | [`mcp-agent/`](./mcp-agent/) |
| CrewAI multi-agent crew | [`crewai-fanout/`](./crewai-fanout/) |
| AutoGen ConversableAgent / GroupChat | [`autogen-branch/`](./autogen-branch/) |
| OpenAI Swarm / Agents SDK | [`openai-swarm/`](./openai-swarm/) |
| LangGraph ReAct (the front-page demo) | [`langgraph-react/`](./langgraph-react/) |

## How rootfs recipes work

Rootfs recipes turn a public Docker / OCI image into a forkd parent
snapshot. Same shape across all of them:

```bash
# 1. Build a parent rootfs from an upstream image
sudo bash recipes/<name>/build.sh

# 2. Snapshot the warmed parent (one-time per image version)
sudo forkd snapshot --tag <name> \
    --kernel /var/lib/forkd/kernels/vmlinux \
    --rootfs recipes/<name>/parent.ext4 \
    --tap forkd-tap0

# 3. Fork N children, fan-out workload
sudo -E forkd fork --tag <name> -n 100 --per-child-netns
```

The first-time `build.sh` of each recipe takes a few minutes
(pulling the Docker image + converting to ext4). The snapshot step
is ~10 s. After that, forking children is the published benchmark
cost.

### Available rootfs recipes

| Recipe | Parent image | Size | Best for |
|---|---|---|---|
| [`python-numpy/`](./python-numpy/) | `python:3.12-slim` + `python3-numpy` | ~1.5 GB | **The canonical fan-out benchmark** — what the chart on the front README measures |
| [`postgres-fixture/`](./postgres-fixture/) | `postgres:16` (initdb done, postmaster pre-launched) | ~500 MB | **Fork-per-test isolated databases** — each child gets a ready-to-query postgres in ~10 ms vs ~2 s for fresh initdb |
| [`ci-parallel-pytest/`](./ci-parallel-pytest/) | `python:3.12-slim` + numpy/pandas/sklearn + your test suite | ~2 GB | **CI test fan-out** — parallel pytest workers without per-worker container cold-start |
| [`playwright-browser/`](./playwright-browser/) | `mcr.microsoft.com/playwright` (Node + Chromium pre-warmed) | ~2.5 GB | **Browser automation farms** — warmed headless Chromium at ~10 ms instead of ~2 s. **Alpha** |
| [`jupyter-kernel/`](./jupyter-kernel/) | `quay.io/jupyter/scipy-notebook` | ~3 GB | **Code-interpreter / notebook agents** — full SciPy stack pre-imported, ~1 ms per fresh kernel |
| [`e2b-codeinterpreter/`](./e2b-codeinterpreter/) | `e2bdev/code-interpreter` | ~600 MB | **AI code-execution agents** (Anthropic / OpenAI tutorials use this image). Lightest "agent ready" option |
| [`coding-agent/`](./coding-agent/) | `python:3.12` + git + ruff + black + pytest | ~1.8 GB | **SWE-style coding agents** that need a real dev toolchain inside the sandbox |
| [`nodejs/`](./nodejs/) | `node:22-slim` | ~250 MB | **JavaScript / TypeScript workloads** (Jest, Playwright fan-out, scraping) |
| [`agent-workbench/`](./agent-workbench/) | `agent-infra/sandbox` (browser + VSCode + Jupyter + MCP + shell) | ~5 GB | **Kitchen-sink workbench** when you want every tool already mounted |

## Notes

- Recipes are tested on Ubuntu 24.04 / Linux 6.14 / x86_64. Other distros
  may need adjustments to `scripts/build-rootfs.sh`.
- Each recipe is self-contained — pick one, run it; you don't need to
  understand the others.
- The "AI agent" framing on the project front page is the dominant use
  case **today** but not the only one — the technology is `fork(2)` for
  KVM microVMs. If your workload needs N hardware-isolated children
  spawned from a warmed parent in milliseconds, forkd is the primitive.
