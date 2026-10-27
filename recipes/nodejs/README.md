# `nodejs`

A minimal Node.js parent. The smallest "real runtime" recipe in the
collection (~250 MB) — useful for JS / TS workloads where Python
isn't needed.

## When to pick this

- Your agent generates and runs JavaScript / TypeScript.
- You're fanning out **Playwright** browser sessions for scraping or
  testing.
- You want to run user-supplied Node.js / Deno scripts in isolation.
- You want the smallest possible parent rootfs (faster snapshot, less
  divergence pressure at high N).

## What you get

- `node:22-slim` base
- `npm` (and yarn via npm if desired)
- sandstorm-init.sh + sandstorm-agent.py as PID 1
- Empty `/workspace` directory for user code

Total rootfs: **~250 MB**, smallest of the recipes.

## Use it

```bash
sudo bash recipes/nodejs/build.sh
sudo bash scripts/host-tap.sh
sudo sandstorm snapshot --tag node \
    --kernel ./vmlinux-6.1.141 \
    --rootfs recipes/nodejs/parent.ext4 \
    --tap sandstorm-tap0

sudo bash scripts/netns-setup.sh 20
sudo -E sandstorm fork --tag node -n 20 --per-child-netns

# Each child can node -e or run scripts
sudo sandstorm exec --child sandstorm-child-3 -- \
    node -e "console.log(process.versions)"
```
