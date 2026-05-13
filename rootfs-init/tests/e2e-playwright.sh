#!/usr/bin/env bash
# End-to-end verification of recipes/playwright-browser/ on a dev box.
# Pulls main, rebuilds forkd binaries, runs the recipe build, snapshots
# the warmed Chromium parent, forks N children, and exercises Sandbox
# .eval() against one of them. Run on the dev box, not the workstation.
set -euo pipefail

REPO=${REPO:-$HOME/forkd}
N=${N:-5}
KERNEL=${KERNEL:-$HOME/work/fc-quickstart/vmlinux-6.1.141}
LOG=/tmp/e2e-playwright.log

# Non-interactive ssh sessions don't pick up ~/.bashrc; source cargo env.
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"

cd "$REPO"

log() { printf "\n==> %s\n" "$*" | tee -a "$LOG"; }

{
  log "git pull main"
  git fetch origin main && git checkout main && git reset --hard origin/main
  git rev-parse --short HEAD

  log "cargo build --release"
  cargo build --release

  log "install forkd binaries (skip if no passwordless sudo)"
  if sudo -n true 2>/dev/null; then
    sudo install -m 0755 target/release/forkd /usr/local/bin/forkd
    sudo install -m 0755 target/release/forkd-controller /usr/local/bin/forkd-controller || true
  else
    echo "  (passwordless sudo not available — assuming forkd is already on PATH)"
    if ! command -v forkd >/dev/null; then
      echo "  forkd not on PATH. Either configure NOPASSWD for sudo or"
      echo "  run this manually: sudo install -m 0755 target/release/forkd /usr/local/bin/forkd"
      exit 2
    fi
  fi

  log "host tap"
  sudo bash scripts/host-tap.sh || true

  log "build playwright-browser rootfs"
  time sudo bash recipes/playwright-browser/build.sh

  log "snapshot warmed parent"
  time sudo forkd snapshot --tag pwb \
      --kernel "$KERNEL" \
      --rootfs "$REPO/recipes/playwright-browser/parent.ext4" \
      --tap forkd-tap0 \
      --boot-wait-secs 25

  log "netns setup for N=$N"
  sudo bash scripts/netns-setup.sh "$N"

  log "fork $N children"
  time sudo -E forkd fork --tag pwb -n "$N" --per-child-netns --memory-limit-mib 1024 --settle-secs 60 &
  FORK_PID=$!
  sleep 8

  log "ping child 1"
  printf '%s\n' '{"action":"ping"}' | nc -q1 10.42.0.2 8888 || true

  log "sb.eval(page.title) via child"
  printf '%s\n' '{"action":"eval","code":"await page.goto(\"https://example.com\"); return await page.title()"}' | nc -q3 10.42.0.2 8888 || true

  log "done"
  wait $FORK_PID 2>/dev/null || true
} 2>&1 | tee -a "$LOG"
