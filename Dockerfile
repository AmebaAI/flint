# syntax=docker/dockerfile:1

FROM rust:1.99.0-bookworm@sha256:114c7a4425406451c2866b6aafe69fe29b1b298832db1277d411ac73c82d04d6 AS build
WORKDIR /build

COPY Cargo.toml Cargo.lock ./
RUN mkdir src \
    && printf 'fn main() {}\n' > src/main.rs \
    && : > src/lib.rs \
    && cargo build --locked --release --bin flint \
    && rm -rf src

COPY src ./src
RUN find src -type f -exec touch {} + \
    && cargo build --locked --release --bin flint

FROM debian:bookworm-slim@sha256:734a72b3d6142a71993b4bc580ca01d8ca687f77a2c422d96a961d3376b179bc
ARG VERSION=0.0.0
LABEL org.opencontainers.image.title="Flint" \
      org.opencontainers.image.vendor="Ameba" \
      org.opencontainers.image.licenses="GPL-3.0-only" \
      org.opencontainers.image.version="$VERSION"

# Upgrade the base image's packages so security fixes published after the pinned
# digest (which can lag the security archive) are installed.
RUN apt-get update \
    && apt-get upgrade --no-install-recommends -y \
    && apt-get install --no-install-recommends -y ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

COPY --from=build /build/target/release/flint /usr/local/bin/flint

ENV PORT=8080
EXPOSE 8080
HEALTHCHECK --interval=2s --timeout=2s --retries=30 CMD curl --fail --silent http://127.0.0.1:8080/_local/health || exit 1
ENTRYPOINT ["/usr/local/bin/flint"]
