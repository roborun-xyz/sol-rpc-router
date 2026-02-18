# Stage 1: Compute a recipe file
FROM lukemathwalker/cargo-chef:latest-rust-1-slim-bookworm AS chef
WORKDIR /app

# Stage 2: Cache dependencies
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# Stage 3: Build the binary
FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
# Build dependencies - this is the caching Docker layer!
RUN cargo chef cook --release --recipe-path recipe.json
# Build application
COPY . .
RUN cargo build --release --bin sol-rpc-router --bin rpc-admin

# Stage 4: Runtime
FROM debian:bookworm-slim AS runtime
WORKDIR /app
# Install OpenSSL/Ca-certificates (required for HTTPS upstream requests)
RUN apt-get update -y \
    && apt-get install -y --no-install-recommends openssl ca-certificates \
    && apt-get autoremove -y \
    && apt-get clean -y \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/sol-rpc-router /usr/local/bin/
COPY --from=builder /app/target/release/rpc-admin /usr/local/bin/

EXPOSE 8080 8081 9090

ENTRYPOINT ["/usr/local/bin/sol-rpc-router"]
CMD ["--config", "/app/config.toml"]
