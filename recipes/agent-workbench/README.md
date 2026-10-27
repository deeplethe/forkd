# `agent-workbench`

A kitchen-sink agent environment — Chromium + VNC + VSCode Server +
Jupyter + shell + MCP server hub, all in one parent.

This recipe wraps **[agent-infra/sandbox](https://github.com/agent-infra/sandbox)**'s
published image. It's the heaviest recipe in the collection; pick it
only if you actually need every battery in one box.

## When to pick this

- Your agent navigates web pages (Chrome devtools, VNC for the user to
  watch).
- Your agent edits code through a real VSCode Server UI.
- You want **all** tools — browser, IDE, Jupyter — without curating
  your own image.

## When NOT to pick this

- You're fanning out at high N (>20). The heavy memory footprint
  eats into the CoW sharing budget; you'll see less benefit than
  with smaller recipes.
- You only need Python code execution → use
  [`e2b-codeinterpreter/`](../e2b-codeinterpreter/) instead.
- You're benchmarking → use [`python-numpy/`](../python-numpy/).

## What you get

- Headless Chromium + Chrome DevTools Protocol + VNC
- VSCode Server (open in browser)
- Jupyter (Python + Node.js kernels)
- Shell with session management
- MCP server hub with pre-configured tools
- File system / terminal / port-forwarding APIs

Total rootfs: **~5 GB** (heaviest of the recipes).

## Use it

```bash
sudo bash recipes/agent-workbench/build.sh
sudo bash scripts/host-tap.sh
sudo sandstorm snapshot --tag wb \
    --kernel ./vmlinux-6.1.141 \
    --rootfs recipes/agent-workbench/parent.ext4 \
    --tap sandstorm-tap0

# Fork modestly — 5 sandboxes, not 100
sudo bash scripts/netns-setup.sh 5
sudo -E sandstorm fork --tag wb -n 5 --per-child-netns --memory-limit-mib 1024

# Each child has a full agent workbench
sudo sandstorm exec --child sandstorm-child-1 -- \
    bash -c "curl -fsS http://localhost:8080/v1/shell/run \
             -d '{\"command\":\"ls /workspace\"}'"
```

## Credit

The image we package as the parent is built by the
[agent-infra/sandbox](https://github.com/agent-infra/sandbox) team
(Apache 2.0). sandstorm just wraps their work in a fork-from-warm
primitive — if you want different tools inside the box, build your
own with their Dockerfile.
