#!/usr/bin/env bash
# setup-host.sh — prepare a Linux host for forkd development.
# Tested on: Ubuntu 24.04 (x86_64). Other distros: PRs welcome.

set -euo pipefail

say() { printf "\033[1;34m==>\033[0m %s\n" "$*"; }
die() { printf "\033[1;31merror:\033[0m %s\n" "$*" >&2; exit 1; }

say "Checking hardware virtualization support..."
if [ "$(grep -Ec '(vmx|svm)' /proc/cpuinfo)" -eq 0 ]; then
    die "CPU does not advertise VT-x / AMD-V. forkd needs KVM."
fi

say "Checking /dev/kvm..."
if [ ! -e /dev/kvm ]; then
    die "/dev/kvm missing. Load the kvm / kvm_intel / kvm_amd kernel modules."
fi
if [ ! -w /dev/kvm ]; then
    say "Adding $USER to the kvm group (you'll need to log out + back in)..."
    sudo usermod -aG kvm "$USER"
fi

say "Installing apt dependencies..."
sudo apt-get update
sudo apt-get install -y \
    build-essential \
    pkg-config \
    libssl-dev \
    curl \
    qemu-utils \
    iproute2 \
    bridge-utils \
    iptables \
    socat \
    jq

say "Installing Rust (if missing)..."
# curl-pipe-sh is the upstream-recommended rustup install path. The
# rustup binary version that lands here does NOT determine what compiler
# forkd actually builds with — `rust-toolchain.toml` at the repo root
# pins the channel (currently `stable`), and rustup fetches that
# toolchain on first `cargo build`. So a supply-chain compromise of
# `sh.rustup.rs` would still be bounded by what rustup-init does
# locally; the project itself remains pinned.
# See #236 for the security discussion. Future work: a `--paranoid` mode
# that downloads the rustup-init binary and verifies sha256 before
# executing. Not done now because the sha256 needs to be refreshed on
# every rustup-init release, which trades supply-chain hygiene for
# maintenance staleness.
if ! command -v cargo >/dev/null; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    # shellcheck disable=SC1091
    source "$HOME/.cargo/env"
fi

FC_VERSION="v1.10.1"
ARCH="$(uname -m)"
say "Installing Firecracker $FC_VERSION ($ARCH)..."
mkdir -p "$HOME/.local/bin"
if [ ! -x "$HOME/.local/bin/firecracker" ]; then
    TMP="$(mktemp -d)"
    curl -fsSL "https://github.com/firecracker-microvm/firecracker/releases/download/${FC_VERSION}/firecracker-${FC_VERSION}-${ARCH}.tgz" \
        | tar -xz -C "$TMP"
    install -m 0755 "$TMP/release-${FC_VERSION}-${ARCH}/firecracker-${FC_VERSION}-${ARCH}" "$HOME/.local/bin/firecracker"
    install -m 0755 "$TMP/release-${FC_VERSION}-${ARCH}/jailer-${FC_VERSION}-${ARCH}" "$HOME/.local/bin/jailer"
    rm -rf "$TMP"
fi

case ":$PATH:" in
    *":$HOME/.local/bin:"*) ;;
    *) say "Add $HOME/.local/bin to PATH (echo 'export PATH=\"\$HOME/.local/bin:\$PATH\"' >> ~/.bashrc)";;
esac

say "Enabling KSM (kernel same-page merging)..."
echo 1    | sudo tee /sys/kernel/mm/ksm/run            >/dev/null
echo 200  | sudo tee /sys/kernel/mm/ksm/sleep_millisecs >/dev/null
echo 1000 | sudo tee /sys/kernel/mm/ksm/pages_to_scan   >/dev/null

say "Reserving 1 GiB of hugepages (adjust as needed)..."
echo 512 | sudo tee /proc/sys/vm/nr_hugepages >/dev/null

say "Done."
echo
echo "Next:"
echo "  1. firecracker --version                  # verify install"
echo "  2. sudo bash scripts/host-tap.sh          # provision forkd-tap0"
echo "  3. sudo bash scripts/build-rootfs.sh ...  # build a parent rootfs"
echo "  4. See README.md → Quick start"
