# Multi-stage build for the sandstormd daemon.
#
# The controller is the long-running daemon; this image runs it as PID 1.
# It does NOT contain Firecracker or KVM tooling — those must be available
# on the host kernel. The container is expected to run with
# `--privileged --network=host --pid=host` (or equivalent CAP_NET_ADMIN +
# CAP_SYS_ADMIN + KVM access) so the daemon can manage netns and cgroups.
#
# Build:   docker build -t sandstormd .
# Run:     docker run -d --name sandstorm --privileged --network=host \
#              -v /var/lib/sandstorm:/var/lib/sandstorm \
#              -v /var/log/sandstorm:/var/log/sandstorm \
#              -v $HOME/.local/share/sandstorm/snapshots:/var/lib/sandstorm/snapshots:ro \
#              sandstormd

# ---- builder ----
FROM rust:1.83-slim-bookworm AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock* rust-toolchain.toml ./
COPY crates ./crates
# `experiments/*` are not needed by `--bin sandstormd`, but the
# workspace Cargo.toml lists them in `members` so Cargo refuses to
# parse the manifest without their per-crate Cargo.toml present. Copy
# the whole dir — it's small and avoids drifting a workspace-stripping
# shim that the rest of the repo wouldn't see.
COPY experiments ./experiments
RUN cargo build --release --bin sandstormd

# ---- runtime ----
FROM debian:bookworm-slim AS runtime
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates iproute2 \
 && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/sandstormd /usr/local/bin/sandstormd

# Non-root by default (the operator can override via --user 0 when the
# daemon needs CAP_NET_ADMIN for netns / cgroup work).
RUN groupadd --system sandstorm \
 && useradd  --system --gid sandstorm --home-dir /var/lib/sandstorm --shell /usr/sbin/nologin sandstorm \
 && mkdir -p /var/lib/sandstorm /var/log/sandstorm \
 && chown -R sandstorm:sandstorm /var/lib/sandstorm /var/log/sandstorm

USER sandstorm
EXPOSE 8889
ENTRYPOINT ["/usr/local/bin/sandstormd", "serve"]
CMD ["--bind", "0.0.0.0:8889"]
