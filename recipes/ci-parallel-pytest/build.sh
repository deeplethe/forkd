#!/usr/bin/env bash
# Build a forkd parent rootfs for CI test parallelism — pytest +
# numpy/pandas/sklearn pre-imported, the demo test project under
# /opt/test_project. Children fork from this, each running a slice
# of the test suite from the warmed parent.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

IMAGE="${IMAGE:-python:3.12-slim}"
SIZE_MIB="${SIZE_MIB:-2048}"
OUT="$SCRIPT_DIR/parent.ext4"

[ "$(id -u)" -eq 0 ] || { echo "run as root" >&2; exit 1; }

# Heavy deps baked in so children inherit the import cost. Pinned so
# the benchmark numbers in README.md stay reproducible across builds.
PIP_PKGS="pytest==8.3.4 numpy==2.0.2 pandas==2.2.3 scikit-learn==1.5.2"

WRAPPED_TAG="forkd-ci-pytest:tmp-$$"
TMP_CTX="$(mktemp -d)"
trap "rm -rf '$TMP_CTX' && docker image rm -f '$WRAPPED_TAG' >/dev/null 2>&1 || true" EXIT

# Copy the test project into the build context so it's baked into
# the rootfs at /opt/test_project. Real users would `cp -r` their
# own project here instead.
cp -r "$SCRIPT_DIR/test_project" "$TMP_CTX/test_project"

cat > "$TMP_CTX/Dockerfile" <<DOCKER
FROM ${IMAGE}
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential \
 && rm -rf /var/lib/apt/lists/*
RUN pip install --no-cache-dir ${PIP_PKGS}
COPY test_project /opt/test_project
WORKDIR /opt/test_project
# Pre-warm: import the heavy deps so they live in the snapshot's
# page cache. Children inherit the warmed mappings via mmap CoW.
RUN python3 -c "import numpy, pandas, sklearn; print('prewarm:', numpy.__version__, pandas.__version__, sklearn.__version__)"
DOCKER

docker build -t "$WRAPPED_TAG" "$TMP_CTX"

bash "$REPO_ROOT/scripts/build-rootfs.sh" "$WRAPPED_TAG" "$OUT" "$SIZE_MIB"

echo
echo "parent rootfs ready: $OUT ($(du -h "$OUT" | cut -f1))"
echo
echo "next:"
echo "  sudo forkd snapshot --tag ci-pytest \\"
echo "      --kernel /var/lib/forkd/kernels/vmlinux \\"
echo "      --rootfs $OUT --tap forkd-tap0"
echo
echo "then run the fan-out demo:"
echo "  FORKD_TOKEN=\$(cat /tmp/bench-pause/token) python3 $SCRIPT_DIR/demo.py"
