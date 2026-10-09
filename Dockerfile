# syntax=docker/dockerfile:1.7

# ---- build stage -----------------------------------------------------------
FROM rust:1.93-bookworm AS builder
WORKDIR /app

RUN apt-get update \
 && apt-get install -y --no-install-recommends pkg-config libssl-dev \
 && rm -rf /var/lib/apt/lists/*

# Cache dependencies separately from source so edits rebuild fast.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src/bin \
 && echo 'fn main() {}' > src/main.rs \
 && echo '' > src/lib.rs \
 && echo 'fn main() {}' > src/bin/rpc-admin.rs \
 && echo 'fn main() {}' > src/bin/benchmark.rs \
 && cargo build --release --locked \
 && rm -rf src

COPY src ./src
RUN touch src/main.rs src/lib.rs src/bin/*.rs \
 && cargo build --release --locked --bin sol-rpc-router --bin rpc-admin

# ---- runtime stage ---------------------------------------------------------
FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates libssl3 curl \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --home /app router
WORKDIR /app

COPY --from=builder /app/target/release/sol-rpc-router /usr/local/bin/sol-rpc-router
COPY --from=builder /app/target/release/rpc-admin /usr/local/bin/rpc-admin

USER router
ENV RUST_LOG=info \
    RPC_ROUTER_CONFIG=/app/config.toml

EXPOSE 28899 28900 28901
HEALTHCHECK --interval=15s --timeout=3s --start-period=5s --retries=3 \
  CMD curl -fsS http://127.0.0.1:28899/health >/dev/null || exit 1

ENTRYPOINT ["sol-rpc-router"]
