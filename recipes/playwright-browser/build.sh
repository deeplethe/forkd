#!/usr/bin/env bash
# Build a forkd parent rootfs from the official Microsoft Playwright
# image. The image ships Node.js + Playwright + Chromium + Firefox +
# WebKit + all dependency .so files preinstalled — saving ~150 s of
# `npx playwright install` work per build.
#
# Parent rootfs is ~2.5 GB; memory.bin after warm-up with a single
# Chromium tab open ≈ 1.5 GiB (vs ~3 GB peak for the bigger
# agent-workbench recipe).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

# Pinning a specific tag so the snapshot is reproducible. Bump on
# Playwright minor releases; CDN protocol changes are rare across
# patch versions.
IMAGE="${IMAGE:-mcr.microsoft.com/playwright:v1.50.0-jammy}"
SIZE_MIB="${SIZE_MIB:-4096}"
OUT="$SCRIPT_DIR/parent.ext4"

[ "$(id -u)" -eq 0 ] || { echo "run as root" >&2; exit 1; }

echo "==> building rootfs from $IMAGE (~2.5 GB image; first pull may take several minutes)"
bash "$REPO_ROOT/scripts/build-rootfs.sh" "$IMAGE" "$OUT" "$SIZE_MIB"

# Drop a tiny warm-up script into the rootfs. forkd-init.sh execs
# forkd-agent.py which evaluates this on startup; the goal is to have
# a headless Chromium with a single about:blank tab already running
# in the parent before the snapshot is taken, so every child inherits
# the warmed Chromium process via mmap CoW.
ROOTFS_MNT=$(mktemp -d)
mount -o loop "$OUT" "$ROOTFS_MNT"
trap "umount '$ROOTFS_MNT' 2>/dev/null; rmdir '$ROOTFS_MNT'" EXIT

cat >"$ROOTFS_MNT/opt/forkd-warmup.js" <<'JS'
// Spawned by forkd-agent.py before snapshot. Launches headless
// Chromium with one about:blank page, signals readiness, then
// serves a line-based JSON command loop over stdin/stdout. The
// agent multiplexes Sandbox.eval(<js>) calls into this loop.
//
// Protocol (one JSON object per line, both directions):
//   ready:   {"ready": true}                                  (warmup → agent, once)
//   request: {"id": "<n>", "code": "<js>"}                    (agent → warmup)
//   reply:   {"id": "<n>", "result": <json>}                  (warmup → agent)
//   error:   {"id": "<n>", "error": "<msg>", "stack": "..."}  (warmup → agent)
//
// `code` is evaluated as an async function body with
// (browser, context, page) in scope. The function's return value
// becomes `result`. Top-level await is supported.
const readline = require('readline');
const { chromium } = require('playwright');

const AsyncFunction = Object.getPrototypeOf(async function () {}).constructor;

(async () => {
  const browser = await chromium.launch({
    headless: true,
    args: ['--no-sandbox', '--disable-gpu', '--disable-dev-shm-usage']
  });
  const context = await browser.newContext();
  const page = await context.newPage();
  await page.goto('about:blank');

  // Diagnostic chatter belongs on stderr — stdout is the protocol channel.
  process.stderr.write('warmup: chromium launched, page=about:blank\n');

  // Ready handshake. After this, every stdout line is a reply.
  process.stdout.write(JSON.stringify({ ready: true }) + '\n');

  const rl = readline.createInterface({ input: process.stdin });
  rl.on('line', async (line) => {
    let req;
    try {
      req = JSON.parse(line);
    } catch (e) {
      process.stdout.write(JSON.stringify({ error: 'invalid json: ' + e.message }) + '\n');
      return;
    }
    try {
      const fn = new AsyncFunction('browser', 'context', 'page', req.code);
      const result = await fn(browser, context, page);
      process.stdout.write(
        JSON.stringify({ id: req.id, result: result === undefined ? null : result }) + '\n'
      );
    } catch (e) {
      process.stdout.write(
        JSON.stringify({ id: req.id, error: e.message, stack: e.stack }) + '\n'
      );
    }
  });

  rl.on('close', async () => {
    await browser.close().catch(() => {});
    process.exit(0);
  });
})().catch((e) => {
  process.stderr.write('warmup fatal: ' + e.stack + '\n');
  process.exit(1);
});
JS

cat >"$ROOTFS_MNT/etc/forkd-recipe.env" <<'ENV'
# forkd-init.sh reads this before launching the agent. The agent
# (forkd-agent.py) forks the warmup process so it's already running
# when the snapshot is taken.
FORKD_WARMUP_CMD="node /opt/forkd-warmup.js"
FORKD_AGENT_LANG="node"
ENV

sync
umount "$ROOTFS_MNT"
rmdir "$ROOTFS_MNT"
trap - EXIT

echo
echo "parent rootfs ready: $OUT ($(du -h "$OUT" | cut -f1))"
echo
echo "next:"
echo "  sudo forkd snapshot --tag pwb --kernel <vmlinux> --rootfs $OUT \\"
echo "      --tap forkd-tap0 --boot-wait-secs 25"
echo
echo "tip: --boot-wait-secs 25 gives Chromium time to fully init"
echo "the renderer process and resolve about:blank before snapshot."
