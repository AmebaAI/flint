# syntax=docker/dockerfile:1

FROM rust:1.96.0-bookworm@sha256:5e2214abe154fe26e39f64488952e5c991eeed1d6d6da7cc8381ae83927f0cfc AS build
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

FROM debian:bookworm-slim@sha256:abd67ffcfa541b485a3dff59865ab629aa048a6c613e639d36e7456b0b229241
ARG VERSION=0.0.0
LABEL org.opencontainers.image.title="Flint" \
      org.opencontainers.image.vendor="Ameba" \
      org.opencontainers.image.licenses="GPL-3.0-only" \
      org.opencontainers.image.version="$VERSION"

RUN apt-get update \
    && apt-get install --no-install-recommends -y ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

COPY --from=build /build/target/release/flint /usr/local/bin/flint

ENV PORT=8080
EXPOSE 8080
HEALTHCHECK --interval=2s --timeout=2s --retries=30 CMD curl --fail --silent http://127.0.0.1:8080/_local/health || exit 1
ENTRYPOINT ["/usr/local/bin/flint"]
