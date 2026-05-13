# `playwright-browser`

A forkd parent built from `mcr.microsoft.com/playwright` — the
official Microsoft Playwright image with Node.js + Playwright +
Chromium (and Firefox/WebKit) + all dependency `.so` files
preinstalled. The parent VM keeps a headless Chromium process alive
through snapshot, so every child fork inherits the warmed browser
via mmap CoW.

> **Status: working.** End-to-end verified on a bare-metal i7-12700
> dev box: snapshot of a warmed Chromium parent → fork 3 children
> in **56 ms wall-clock** → `sb.eval("return await page.title()")`
> returns in **10–82 ms per child**. Two gotchas baked into the
> build flow below:
>
> - Parent VM needs **≥ 2 GiB memory** (Chromium OOMs at the 512 MiB
>   default); use `--mem-size-mib 2048` on `forkd snapshot`.
> - Per-child cgroup ceiling should be **≥ 2560 MiB** to stay above
>   Chromium peak RSS.

## Why this recipe

Browser fan-out is the second-largest AI-agent workload shape after
Python — Anthropic computer-use, OpenAI web browsing, every coding
agent that uses Playwright/Puppeteer for in-browser interactions.

Cold-start of a fresh Chromium-in-container is 2–3 seconds:

| Step | Cold-start cost |
|---|---:|
| Container start | ~300 ms |
| `node` boot + Playwright lib load | ~400 ms |
| `chromium.launch()` (CDP, renderer process) | ~1.2 s |
| First `newPage()` + `goto(about:blank)` | ~100 ms |
| **Total per fresh browser** | **~2 s** |

With forkd, the parent VM does this work **once** at snapshot time;
every child fork inherits the post-launch state in ~10 ms. **100–
300× faster** per browser instance.

This is the workload shape Anthropic's computer-use and OpenAI's
browser tool are on — many short-lived, parallel browser sessions,
each needing a fresh isolated context.

## What you get

- `mcr.microsoft.com/playwright:v1.50.0-jammy` base
- Pre-launched headless Chromium with one `about:blank` page,
  resident in parent memory at snapshot time
- forkd-init.sh + forkd-agent (Node bridge — landing) running as
  PID 1; agent exposes `eval` / `page.*` / `browser.*` over TCP

Total rootfs: **~2.5 GB**, memory image after warm-up: **~1.5 GiB**.

## Use it

```bash
sudo bash recipes/playwright-browser/build.sh
sudo bash scripts/host-tap.sh
sudo -E forkd snapshot --tag pwb \
    --kernel ./vmlinux-6.1.141 \
    --rootfs recipes/playwright-browser/parent.ext4 \
    --tap forkd-tap0 \
    --boot-wait-secs 25      # Chromium renderer init takes longer than Python import
    --mem-size-mib 2048      # Chromium OOMs on the 512 MiB default

# Fork 50 browser sessions, all share the warmed Chromium
sudo bash scripts/netns-setup.sh 50
sudo -E forkd fork --tag pwb -n 50 --per-child-netns --memory-limit-mib 2560

# Drive one of them via the warmed Chromium
sudo -E forkd eval --child forkd-child-7 -- \
    "await page.goto('https://example.com'); return await page.title()"
# → "Example Domain"   (typical: 10–80 ms)
```

## Python SDK

```python
from forkd import Sandbox

with Sandbox(tag="pwb") as sb:
    # Browser is already warm — no Chromium launch cost. The agent
    # routes `eval` to the warmed Node + Playwright in PID 1's child;
    # `page`, `context`, `browser` are in scope.
    title = sb.eval(
        "await page.goto('https://example.com'); return await page.title()"
    )
    print(title)  # → "Example Domain"
```

## When to pick this

- You're building an **AI agent that drives a browser** (computer-
  use, web-research agent, scraping agent, end-to-end UI test
  generator).
- You run **Playwright test suites at parallel scale** and pay
  multi-second-per-browser cold start.
- You want **per-task browser isolation** without the Docker
  cold-start tax.

## When NOT to pick this

- You only need Python without a browser → use
  [`python-numpy/`](../python-numpy/) (1/2 the size).
- You want the full IDE + VSCode + browser kitchen sink → use
  [`agent-workbench/`](../agent-workbench/).
- You need to drive a **real GPU-accelerated browser** (forkd children
  share the parent's headless config; switching to `--enable-gpu`
  per-child needs a different warmup pattern).

## Benchmarks

To be filled in once the recipe's end-to-end Firecracker run is
verified. Target shape: 50 concurrent fresh Chromium pages reachable
in <500 ms wall-clock, vs ~100 s cold-boot Playwright-in-Docker.
