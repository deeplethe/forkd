# Multi-stage build for the forkd-controller binary.
#
# The controller is the long-running daemon; this image runs it as PID 1.
# It does NOT contain Firecracker or KVM tooling — those must be available
# on the host kernel. The container is expected to run with
# `--privileged --network=host --pid=host` (or equivalent CAP_NET_ADMIN +
# CAP_SYS_ADMIN + KVM access) so the daemon can manage netns and cgroups.
#
# Build:   docker build -t forkd-controller .
# Run:     docker run -d --name forkd --privileged --network=host \
#              -v /var/lib/forkd:/var/lib/forkd \
#              -v /var/log/forkd:/var/log/forkd \
#              -v $HOME/.local/share/forkd/snapshots:/var/lib/forkd/snapshots:ro \
#              forkd-controller

# ---- builder ----
FROM rust:1.83-slim-bookworm AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock* rust-toolchain.toml ./
COPY crates ./crates
# `experiments/*` are not needed by `--bin forkd-controller`, but the
# workspace Cargo.toml lists them in `members` so Cargo refuses to
# parse the manifest without their per-crate Cargo.toml present. Copy
# the whole dir — it's small and avoids drifting a workspace-stripping
# shim that the rest of the repo wouldn't see.
COPY experiments ./experiments
RUN cargo build --release --bin forkd-controller

# ---- runtime ----
FROM debian:bookworm-slim AS runtime
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates iproute2 \
 && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/forkd-controller /usr/local/bin/forkd-controller

# Non-root by default (the operator can override via --user 0 when the
# daemon needs CAP_NET_ADMIN for netns / cgroup work).
RUN groupadd --system forkd \
 && useradd  --system --gid forkd --home-dir /var/lib/forkd --shell /usr/sbin/nologin forkd \
 && mkdir -p /var/lib/forkd /var/log/forkd \
 && chown -R forkd:forkd /var/lib/forkd /var/log/forkd

USER forkd
EXPOSE 8889
ENTRYPOINT ["/usr/local/bin/forkd-controller", "serve"]
CMD ["--bind", "0.0.0.0:8889"]
