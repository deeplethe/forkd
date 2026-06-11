#!/usr/bin/env bash
# setup-host.sh — prepare a Linux host for forkd development.
# Tested on: Ubuntu 24.04 (x86_64). Other distros: PRs welcome.

set -euo pipefail

say() { printf "\033[1;34m==>\033[0m %s\n" "$*"; }
die() { printf "\033[1;31merror:\033[0m %s\n" "$*" >&2; exit 1; }

usage() {
    cat <<'EOF'
Usage: scripts/setup-host.sh [--paranoid]

Options:
  --paranoid  Download pinned rustup-init and Firecracker archives, verify
              their sha256 sums, then install them. Default behavior is unchanged.
  -h, --help  Show this help text.
EOF
}

PARANOID=0
for arg in "$@"; do
    case "$arg" in
        --paranoid) PARANOID=1 ;;
        -h|--help) usage; exit 0 ;;
        *) usage >&2; die "unknown argument: $arg" ;;
    esac
done

RUSTUP_VERSION="1.29.0"
RUSTUP_X86_64_SHA256="4acc9acc76d5079515b46346a485974457b5a79893cfb01112423c89aeb5aa10"
RUSTUP_AARCH64_SHA256="9732d6c5e2a098d3521fca8145d826ae0aaa067ef2385ead08e6feac88fa5792"

FC_VERSION="v1.10.1"
FC_X86_64_SHA256="36112969952b0e34fadcfca769d48a55dc22cbba99af17e02bd0e24fc35adc77"
FC_AARCH64_SHA256="9e3641071de140979afaac0c52fdc107baeba398bdb5709c12f77ee469207fcd"

TEMP_DIRS=()
cleanup_temp_dirs() {
    if [ "${#TEMP_DIRS[@]}" -eq 0 ]; then
        return
    fi
    rm -rf "${TEMP_DIRS[@]}"
}
trap cleanup_temp_dirs EXIT

make_temp_dir() {
    local -n outvar="$1"
    outvar="$(mktemp -d)"
    TEMP_DIRS+=("$outvar")
}

host_arch() {
    case "$(uname -m)" in
        x86_64) printf "x86_64\n" ;;
        aarch64|arm64) printf "aarch64\n" ;;
        *) die "unsupported architecture for setup-host.sh: $(uname -m)" ;;
    esac
}

rustup_triple() {
    case "$1" in
        x86_64) printf "x86_64-unknown-linux-gnu\n" ;;
        aarch64) printf "aarch64-unknown-linux-gnu\n" ;;
        *) die "unsupported architecture for rustup-init: $1" ;;
    esac
}

rustup_sha256() {
    case "$1" in
        x86_64-unknown-linux-gnu) printf "%s\n" "$RUSTUP_X86_64_SHA256" ;;
        aarch64-unknown-linux-gnu) printf "%s\n" "$RUSTUP_AARCH64_SHA256" ;;
        *) die "missing rustup-init sha256 for $1" ;;
    esac
}

firecracker_sha256() {
    case "$1" in
        x86_64) printf "%s\n" "$FC_X86_64_SHA256" ;;
        aarch64) printf "%s\n" "$FC_AARCH64_SHA256" ;;
        *) die "missing Firecracker sha256 for $1" ;;
    esac
}

download_and_verify() {
    local url="$1"
    local dest="$2"
    local expected="$3"
    local label="$4"
    local actual

    curl -fsSL "$url" -o "$dest"
    actual="$(sha256sum "$dest" | awk '{print $1}')"
    if [ "$actual" != "$expected" ]; then
        die "$label sha256 mismatch: expected $expected, got $actual"
    fi
}

install_rustup_paranoid() {
    local arch triple expected tmp rustup_init
    arch="$(host_arch)"
    triple="$(rustup_triple "$arch")"
    expected="$(rustup_sha256 "$triple")"
    make_temp_dir tmp
    rustup_init="$tmp/rustup-init"

    say "Downloading rustup-init $RUSTUP_VERSION ($triple) with sha256 verification..."
    download_and_verify \
        "https://static.rust-lang.org/rustup/archive/${RUSTUP_VERSION}/${triple}/rustup-init" \
        "$rustup_init" \
        "$expected" \
        "rustup-init ${RUSTUP_VERSION} ${triple}"
    chmod 0755 "$rustup_init"
    "$rustup_init" -y
}

install_firecracker_from_archive() {
    local arch="$1"
    local tmp archive
    make_temp_dir tmp
    archive="$tmp/firecracker-${FC_VERSION}-${arch}.tgz"

    if [ "$PARANOID" -eq 1 ]; then
        say "Downloading Firecracker $FC_VERSION ($arch) with sha256 verification..."
        download_and_verify \
            "https://github.com/firecracker-microvm/firecracker/releases/download/${FC_VERSION}/firecracker-${FC_VERSION}-${arch}.tgz" \
            "$archive" \
            "$(firecracker_sha256 "$arch")" \
            "Firecracker ${FC_VERSION} ${arch}"
        tar -xzf "$archive" -C "$tmp"
    else
        curl -fsSL "https://github.com/firecracker-microvm/firecracker/releases/download/${FC_VERSION}/firecracker-${FC_VERSION}-${arch}.tgz" \
            | tar -xz -C "$tmp"
    fi

    install -m 0755 "$tmp/release-${FC_VERSION}-${arch}/firecracker-${FC_VERSION}-${arch}" "$HOME/.local/bin/firecracker"
    install -m 0755 "$tmp/release-${FC_VERSION}-${arch}/jailer-${FC_VERSION}-${arch}" "$HOME/.local/bin/jailer"
}

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
# toolchain on first `cargo build`. The default remains curl-pipe-sh;
# pass --paranoid to verify a pinned rustup-init binary before running it.
if ! command -v cargo >/dev/null; then
    if [ "$PARANOID" -eq 1 ]; then
        install_rustup_paranoid
    else
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    fi
    # shellcheck disable=SC1091
    source "$HOME/.cargo/env"
fi

ARCH="$(host_arch)"
say "Installing Firecracker $FC_VERSION ($ARCH)..."
mkdir -p "$HOME/.local/bin"
if [ ! -x "$HOME/.local/bin/firecracker" ]; then
    install_firecracker_from_archive "$ARCH"
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
