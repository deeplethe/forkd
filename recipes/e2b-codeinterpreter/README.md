# `e2b-codeinterpreter`

A sandstorm parent built from E2B's official `code-interpreter` template.
This is the image Anthropic's tool-use tutorials, OpenAI's
code-interpreter examples, and most "let the agent run Python"
applications converge on.

## Why this recipe is the headline one

- **Audience overlap is exact.** People who run E2B already use this
  image; people who want fork-from-warm on top can swap sandstorm in
  without re-curating their sandbox tools.
- **sandstorm's Python SDK is E2B wire-compatible.** Code that uses
  `from e2b import Sandbox` can `from sandstorm import Sandbox` and
  run against this parent unchanged.
- **It's tuned for fan-out.** Smaller than agent-infra/sandbox (~600 MB),
  fewer always-running services → faster snapshot + smaller memory.bin
  → more CoW pages shared at N=100.

## What you get

- Python 3 + Jupyter kernel
- `numpy`, `pandas`, `matplotlib`, `scipy`, `scikit-learn`, `requests` preinstalled
- A `/code` working directory configured for the kernel
- sandstorm-init.sh + sandstorm-agent.py wired in as PID 1

Total rootfs: **~600 MB**.

## Use it

```bash
sudo bash recipes/e2b-codeinterpreter/build.sh
sudo bash scripts/host-tap.sh
sudo sandstorm snapshot --tag ci \
    --kernel ./vmlinux-6.1.141 \
    --rootfs recipes/e2b-codeinterpreter/parent.ext4 \
    --tap sandstorm-tap0

# Fan out 50 code-interpreter sandboxes
sudo bash scripts/netns-setup.sh 50
sudo -E sandstorm fork --tag ci -n 50 --per-child-netns --memory-limit-mib 256

# Run pandas analysis in each — picks up the pre-warmed kernel
sudo sandstorm eval --child sandstorm-child-7 -- "pandas.DataFrame({'a':[1,2,3]}).sum().to_dict()"
```

## Python SDK (E2B-compatible)

```python
from sandstorm import Sandbox   # drop-in replacement for `from e2b import Sandbox`

with Sandbox() as sb:
    r = sb.commands.run("python3 -c 'import pandas; print(pandas.__version__)'")
    print(r.stdout)
```

## When to pick this

- You're **building an AI code interpreter** and want fan-out for
  per-conversation isolation.
- You already use E2B and want to self-host with significantly faster
  per-request spawn.
- You want the lightest "agent ready" parent.
