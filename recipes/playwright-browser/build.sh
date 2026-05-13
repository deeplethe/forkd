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
cleanup() {
    umount "$ROOTFS_MNT/proc" 2>/dev/null || true
    umount "$ROOTFS_MNT/sys" 2>/dev/null || true
    umount "$ROOTFS_MNT/dev/pts" 2>/dev/null || true
    umount "$ROOTFS_MNT/dev" 2>/dev/null || true
    umount "$ROOTFS_MNT" 2>/dev/null || true
    rmdir "$ROOTFS_MNT" 2>/dev/null || true
}
trap cleanup EXIT

# The official Playwright image intentionally omits the `playwright`
# JS module — only browser binaries are shipped under /ms-playwright.
# We chroot in and `npm install -g playwright@<matching version>` so
# require('playwright') resolves at warmup time. Browser download is
# skipped via env var because /ms-playwright is already populated.
PLAYWRIGHT_NPM_VERSION="${PLAYWRIGHT_NPM_VERSION:-1.50.0}"
echo "==> bind-mounting /proc /sys /dev for chroot npm install..."
mount -t proc proc "$ROOTFS_MNT/proc"
mount -t sysfs sys "$ROOTFS_MNT/sys"
mount --bind /dev "$ROOTFS_MNT/dev"
[ -d /dev/pts ] && mount --bind /dev/pts "$ROOTFS_MNT/dev/pts" 2>/dev/null || true
cp /etc/resolv.conf "$ROOTFS_MNT/etc/resolv.conf"
echo "==> chroot npm install -g playwright@${PLAYWRIGHT_NPM_VERSION}..."
chroot "$ROOTFS_MNT" /bin/bash -c \
    "PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD=1 \
     PLAYWRIGHT_BROWSERS_PATH=/ms-playwright \
     npm install -g --no-audit --no-fund playwright@${PLAYWRIGHT_NPM_VERSION}"

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
# forkd-agent.py reads this before serving. The warmup is launched
# via `env` so we can inject NODE_PATH — `npm install -g playwright`
# lands the module at /usr/lib/node_modules which isn't in node's
# default require() search path. PLAYWRIGHT_BROWSERS_PATH points the
# JS driver at the pre-shipped Chromium under /ms-playwright.
FORKD_WARMUP_CMD="env NODE_PATH=/usr/lib/node_modules PLAYWRIGHT_BROWSERS_PATH=/ms-playwright node /opt/forkd-warmup.js"
FORKD_AGENT_LANG="node"
ENV

sync
# Unmount proc/sys/dev/pts before the rootfs loopback — `cleanup` does
# the right order, the inline `umount $ROOTFS_MNT` we used to have
# fails with EBUSY because the chroot bind mounts are still active.
cleanup
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
