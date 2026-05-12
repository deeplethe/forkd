# `jupyter-kernel`

A forkd parent built from `quay.io/jupyter/scipy-notebook` — the
canonical Jupyter image with the full SciPy stack preinstalled
(`numpy`, `pandas`, `scipy`, `scikit-learn`, `matplotlib`,
`seaborn`, `sympy`, `ipython`).

## Why this recipe

Jupyter / code-interpreter workloads spend 1–3 seconds per fresh
kernel just on import time: numpy alone is ~300 ms, add pandas +
sklearn and you're at 2 s before the first cell runs. With forkd,
the parent VM does that import work **once** at snapshot time;
every child fork inherits the post-import memory state via mmap CoW.

Net: a "fresh kernel" goes from ~2 s to ~1 ms per child.

This is the shape Anthropic Claude code-interpreter, OpenAI
code-interpreter, and Modal-hosted notebook agents all run on —
many short-lived kernel sessions, each needing the SciPy runtime
ready immediately.

## What you get

- `quay.io/jupyter/scipy-notebook` base
- Full SciPy stack: numpy, pandas, scipy, sklearn, matplotlib,
  seaborn, sympy, ipython
- forkd-init.sh + forkd-agent.py as PID 1; the agent's `eval`
  endpoint runs Python expressions against the warmed interpreter
  (same as Jupyter's kernel does over ZMQ, but simpler JSON over TCP)

Total rootfs: **~3 GB**, memory image after warm-up: **~700 MiB**.

## Use it

```bash
sudo bash recipes/jupyter-kernel/build.sh
sudo bash scripts/host-tap.sh
sudo forkd snapshot --tag jk \
    --kernel ./vmlinux-6.1.141 \
    --rootfs recipes/jupyter-kernel/parent.ext4 \
    --tap forkd-tap0 \
    --boot-wait-secs 20    # SciPy import takes longer than plain Python

# Fork 50 kernel sessions, all share the warmed SciPy stack
sudo bash scripts/netns-setup.sh 50
sudo -E forkd fork --tag jk -n 50 --per-child-netns --memory-limit-mib 512

# Each child can eval pandas / sklearn instantly
sudo forkd eval --child forkd-child-7 -- \
    "pandas.DataFrame({'a':[1,2,3]}).describe().to_dict()"
```

## Python SDK

```python
from forkd import Sandbox

with Sandbox(tag="jk") as sb:
    # First cell: model train, no extra imports needed
    sb.eval("sklearn.datasets.load_iris().data.shape")  # → (150, 4)
    sb.eval("numpy.linalg.eigvals([[1,2],[3,4]]).tolist()")
```

## When to pick this

- You're building an **AI code interpreter** and Python is your
  primary language.
- You run **notebook-style evaluation harnesses** (papermill,
  nbconvert, custom rollouts) and want per-task isolation without
  per-task cold-start.
- You want **JupyterHub-like multi-user kernels** but with sub-second
  spawn instead of multi-second container startup.

If you need full Jupyter kernel ZMQ protocol (rather than forkd's
simpler `eval` channel), the parent kernel can still be started
in the rootfs build — but you'll need to wire ZMQ port forwarding
through the netns, which we don't ship a recipe for yet. See the
JupyterHub spawner tracking issue on GitHub.

## When NOT to pick this

- You only need plain Python without the SciPy stack → use
  [`python-numpy/`](../python-numpy/) (1/2 the size, faster snapshot).
- You're running a coding agent that needs `git` + `pytest` + dev
  tooling → use [`coding-agent/`](../coding-agent/).
