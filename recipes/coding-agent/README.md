# `coding-agent`

A sandstorm parent for **coding agents** — Python 3.12 plus the dev
toolchain a software-engineering agent typically needs: `git`,
`ruff`, `black`, `pytest`, `mypy`, `pip`, `build-essential`, plus a
small `gh` install for GitHub API access.

## When to pick this

- You're running **SWE-bench / SWE-bench-lite style evaluations** —
  hundreds of repository snapshots checked out, tests run in parallel.
- You're building a **coding agent** (Devin-style, OpenHands-style)
  where each task gets its own isolated workspace.
- You need a sandbox that can `git clone`, `pip install`, `pytest`
  without rebuilding the parent.

Compared to `python-numpy/`, this recipe trades ~300 MB more rootfs
for a real dev environment inside every fork.

## What you get

- Python 3.12 + pip
- `git`, `gh` (GitHub CLI), `build-essential`, `make`
- Python tools: `ruff`, `black`, `mypy`, `pytest`, `requests`
- sandstorm-init.sh + sandstorm-agent.py as PID 1

Total rootfs: **~1.8 GB**.

## Use it

```bash
sudo bash recipes/coding-agent/build.sh
sudo bash scripts/host-tap.sh
sudo sandstorm snapshot --tag swe \
    --kernel ./vmlinux-6.1.141 \
    --rootfs recipes/coding-agent/parent.ext4 \
    --tap sandstorm-tap0

# Spawn 50 parallel workspaces for evaluation rollouts
sudo bash scripts/netns-setup.sh 50
sudo -E sandstorm fork --tag swe -n 50 --per-child-netns --memory-limit-mib 512

# Each child can git clone + pytest
sudo sandstorm exec --child sandstorm-child-1 -- \
    bash -c "git clone https://github.com/psf/requests /tmp/r && cd /tmp/r && pytest -q"
```
