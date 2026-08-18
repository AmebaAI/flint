# syntax=docker/dockerfile:1

FROM rust:1.96.0-bookworm AS build
WORKDIR /build

COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --locked --release --bin flint

FROM debian:bookworm-slim
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
