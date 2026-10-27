#!/usr/bin/env bash
# Build a sandstorm parent rootfs for SWE-style coding agents.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

IMAGE="${IMAGE:-python:3.12}"
SIZE_MIB="${SIZE_MIB:-2048}"
OUT="$SCRIPT_DIR/parent.ext4"

[ "$(id -u)" -eq 0 ] || { echo "run as root" >&2; exit 1; }

# Dev toolchain. build-essential pulls gcc + make; gh comes via Ubuntu PPA in
# the python:3.12 (debian-slim base) image.
APT_PKGS="git build-essential make curl ca-certificates"
PIP_PKGS="ruff black mypy pytest requests"

# We use a wrapper image: layer the apt + pip installs on top of python:3.12
# so build-rootfs.sh sees one prepared image.
WRAPPED_TAG="sandstorm-coding-agent:tmp-$$"
TMP_CTX="$(mktemp -d)"
trap "rm -rf '$TMP_CTX' && docker image rm -f '$WRAPPED_TAG' >/dev/null 2>&1 || true" EXIT

cat > "$TMP_CTX/Dockerfile" <<DOCKER
FROM ${IMAGE}
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends ${APT_PKGS} \
 && rm -rf /var/lib/apt/lists/*
RUN curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg \
      | dd of=/usr/share/keyrings/githubcli-archive-keyring.gpg \
 && chmod go+r /usr/share/keyrings/githubcli-archive-keyring.gpg \
 && echo "deb [arch=\$(dpkg --print-architecture) signed-by=/usr/share/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" \
      > /etc/apt/sources.list.d/github-cli.list \
 && apt-get update && apt-get install -y --no-install-recommends gh \
 && rm -rf /var/lib/apt/lists/*
RUN pip install --no-cache-dir ${PIP_PKGS}
DOCKER

docker build -t "$WRAPPED_TAG" "$TMP_CTX"

bash "$REPO_ROOT/scripts/build-rootfs.sh" "$WRAPPED_TAG" "$OUT" "$SIZE_MIB"

echo
echo "parent rootfs ready: $OUT ($(du -h "$OUT" | cut -f1))"
echo "next: sudo sandstorm snapshot --tag swe --kernel <vmlinux> --rootfs $OUT --tap sandstorm-tap0"
