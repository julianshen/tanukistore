# syntax=docker/dockerfile:1

# tanukistore ships as ONE image carrying BOTH binaries:
#   tanukistore-server  - the long-running update server (the default entrypoint)
#   tanukistore-publish - the publishing CLI, run as a one-shot k8s Job
# They share the whole protocol core, so building them together costs one
# compile of `tanukistore-core` instead of two, and it guarantees the server
# and the tool that writes its manifests can never drift to different versions
# of the derivation logic.

# The toolchain is pinned rather than floating on `rust:slim`, because the
# workspace is edition 2024 with resolver 3 - a silent base-image bump to a
# pre-1.85 toolchain would fail in a way that reads like a source error.
#
# Deliberately NOT `--platform=$BUILDPLATFORM`. That would pin the builder to
# the host's architecture while the runtime stage follows --platform, so on an
# arm64 machine building for amd64 it would copy arm64 binaries into an amd64
# image - which builds and pushes cleanly, then crashes with `exec format
# error` only once k8s tries to start it. Letting the builder follow the target
# platform costs emulation time and buys an image that actually runs.
FROM rust:1.91-slim-trixie AS builder

WORKDIR /build

# --- dependency layer -------------------------------------------------------
# Build the dependency graph against STUB sources first. Docker invalidates a
# layer and everything after it when any copied file changes, so copying real
# sources before this point would rebuild every crate in the tree on a
# one-character edit to our own code. Manifests change far more rarely, so
# keying the expensive layer on them alone is what makes an incremental build
# seconds instead of minutes.
COPY Cargo.toml Cargo.lock ./
COPY crates/core/Cargo.toml    crates/core/Cargo.toml
COPY crates/server/Cargo.toml  crates/server/Cargo.toml
COPY crates/publish/Cargo.toml crates/publish/Cargo.toml
RUN mkdir -p crates/core/src crates/server/src crates/publish/src \
 && : > crates/core/src/lib.rs \
 && echo 'fn main() {}' > crates/server/src/main.rs \
 && echo 'fn main() {}' > crates/publish/src/main.rs \
 && cargo build --release --workspace --locked \
 && rm -r crates/core/src crates/server/src crates/publish/src

# --- application layer ------------------------------------------------------
COPY crates crates

# cargo decides what to recompile from mtime, and COPY stamps files with their
# mtime from the build context - which can be OLDER than the stubs just built
# above. Without this touch cargo concludes the stubs are current and ships an
# image whose binaries are `fn main() {}`. This failure is silent: the build
# succeeds and the container starts, it just does nothing.
RUN find crates -name '*.rs' -exec touch {} + \
 && cargo build --release --workspace --locked \
 && strip target/release/tanukistore-server target/release/tanukistore-publish

# --- runtime ----------------------------------------------------------------
# Debian slim rather than distroless or scratch: the dependency set is pure
# Rust with no native TLS, so a static musl build would also work, but slim
# keeps a shell available for `kubectl exec` debugging, which matters more
# while the service is still being brought up. Revisit once it is stable.
FROM debian:trixie-slim AS runtime

# ca-certificates is not optional: the server talks to S3/MinIO over HTTPS and
# without a trust store every object fetch fails certificate verification.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates tini \
 && rm -rf /var/lib/apt/lists/*

# A fixed high UID, not a name: the k8s manifest pins runAsUser numerically,
# and a name would resolve differently if the base image's user table changed.
RUN groupadd --system --gid 10001 tanuki \
 && useradd --system --uid 10001 --gid 10001 --no-create-home \
            --shell /usr/sbin/nologin tanuki

COPY --from=builder /build/target/release/tanukistore-server  /usr/local/bin/tanukistore-server
COPY --from=builder /build/target/release/tanukistore-publish /usr/local/bin/tanukistore-publish

USER 10001:10001

# Documentary only - the server does not bind a port yet. Kept so the eventual
# k8s Service has a declared contract to target rather than a magic number.
EXPOSE 8080

# tini reaps zombies and forwards SIGTERM, so a `kubectl rollout` or an evicted
# pod terminates promptly instead of waiting out terminationGracePeriodSeconds.
ENTRYPOINT ["/usr/bin/tini", "--", "tanukistore-server"]
