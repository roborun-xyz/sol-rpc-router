---
created: 2025-11-13
updated: 2026-10-08
---

# CLAUDE.md

## Project Overview

sol-rpc-router is a Solana JSON-RPC reverse proxy written in Rust. It sits in front of one or more Solana RPC backends and provides API key authentication, per-key rate limiting, weighted load balancing, automatic failover, sendTransaction fan-out, method routing/blocking, WebSocket proxying, consensus-aware health checking, and Prometheus metrics.

## Build & Test

```bash
cargo build                # debug build
cargo build --release      # release build (LTO)
cargo test                 # run all tests (no external deps needed)
cargo fmt                  # format code
cargo clippy --all-targets # lint (CI uses -D warnings)

# optional: real-Redis keystore test
TEST_REDIS_URL=redis://127.0.0.1:6379/15 cargo test --test redis_keystore_test

# benchmark (in-process mock upstream, no Redis)
cargo run --release --bin benchmark -- --concurrency 64 --duration 10

# full stack
cp config.example.toml config.toml && docker compose up -d --build
```

Tests use `MockKeyStore` (no Redis required). Mock HTTP/WS backends bind to `127.0.0.1:0` in-process.

## Project Structure

```
src/
  main.rs           Entry point: CLI args, server setup, SIGHUP reload, graceful shutdown
  config.rs         TOML config structs + load_config()/validate_config()
  state.rs          RouterState (backends, routes, fanout/blocked sets), select_backend()
  router.rs         http_router()/ws_router()/metrics_router(): the ONE place middleware order
                    is defined; binary, benchmark and tests all use it
  handlers.rs       Axum handlers: proxy (retry + fan-out), ws_proxy, health_endpoint
                    Middleware: request_id, require_api_key, extract_rpc_method, log_requests,
                    track_metrics
  upstream.rs       build_uri() (merges backend query strings), forwardable_headers(),
                    is_retryable_status()
  rpc.rs            probe_request() (single + batch), error_response(), KNOWN_METHODS,
                    metric_label()
  health.rs         HealthState, BackendHealthStatus (slot, latency), health_check_loop
  keystore.rs       KeyStore trait, RedisKeyStore (HGETALL + bounded moka cache, Lua limiter),
                    FileKeyStore (config [[api_keys]], in-memory per-second windows, reload())
  mock.rs           MockKeyStore for testing (supports error injection via set_error())
  lib.rs            Module declarations
  bin/rpc-admin.rs  Admin CLI for API key CRUD
  bin/benchmark.rs  In-process benchmark

tests/
  config_test.rs          Config validation paths (incl. keystore selection)
  handler_test.rs         Proxy errors, health endpoint, extract_rpc_method middleware
  proxy_features_test.rs  Header auth, credential stripping, failover, fan-out, blocked
                          methods, batches, request ids, sub-path forwarding
  ws_test.rs              End-to-end WebSocket relay through a real TCP listener
  keystore_test.rs        MockKeyStore behavior
  redis_keystore_test.rs  RedisKeyStore against a real Redis (skips without TEST_REDIS_URL)
  routing_test.rs         Backend selection (HTTP + WebSocket, healthy/unhealthy)
```

## Key Patterns

- **State**: `AppState` is shared via `Arc<AppState>`; `AppState.state` is an `Arc<ArcSwap<RouterState>>` so SIGHUP reloads swap atomically. Build `RouterState` with `from_config()` (prod) or `simple()` (tests/bench).
- **KeyStore trait**: `async fn validate_key(&self, key: &str) -> Result<Option<KeyInfo>, KeyStoreError>`. `Ok(Some)` valid, `Ok(None)` invalid/inactive/expired, `Err(RateLimited)` or `Err(Backend(msg))`.
- **Error codes**: router codes are `-32090..-32099` (`rpc::codes`); never use `-32001..-32016`, Solana nodes own that range.
- **Auth**: `require_api_key` middleware runs BEFORE `extract_rpc_method`, so bodies are never buffered for unauthenticated requests; it stores `ClientOwner` in request extensions and handlers read it (no auth inside handlers). Key sources: `?api-key=`, `x-api-key`, `Authorization: Bearer`. `upstream::forwardable_headers()` strips them before forwarding. Auth errors carry `id: null`.
- **Keystore selection**: `Config::keystore_kind()`; empty `redis_url` + `[[api_keys]]` = file store, otherwise Redis. Setting both is a config error. SIGHUP reloads file keys via `FileKeyStore::reload()`.
- **Secrets in logs**: always pass backend URLs through `upstream::redact_url()` before logging; provider keys live in their query strings.
- **Proxy flow**: auth → blocked check → fan-out (if method listed) → retry loop via `RouterState::select_backend(method, &tried)`. Retryable = transport error, timeout, 408/429/5xx. Non-retryable upstream statuses pass through untouched.
- **Fan-out**: sends are `tokio::spawn`ed and results arrive over an mpsc channel, so dropping the receiver after the first success does not cancel the remaining sends.
- **Errors**: router-generated errors go through `rpc::error_response()` (JSON-RPC body with the request id). Keep HTTP status codes stable; tests assert on them.
- **Health**: probe decisions live in the pure `health::apply_probe()` (unit-tested); the loop only does I/O and logging. `HealthState` (`RwLock<HashMap>`) is the detailed record and what `/health` reports; `RuntimeBackend.healthy` (`AtomicBool`) is the lock-free flag the data path reads. The health loop updates both. Backends default to healthy.
- **Metrics**: use `rpc::metric_label()` for any `rpc_method` label to keep cardinality bounded.
- **Tests**: use `tower::ServiceExt::oneshot()` on routers; bind real listeners only when `ConnectInfo` or a WS client is needed.

## Code Conventions

- Async runtime: tokio; framework: axum 0.7; HTTP client: hyper-util legacy Client with hyper-tls
- Error handling: `Result<T, Box<dyn std::error::Error>>` for config, `Result<T, String>` for keystore
- Logging: tracing crate. Metrics: metrics crate + metrics-exporter-prometheus
- Commits follow Conventional Commits; CI enforces `cargo fmt --check` and `clippy -D warnings`
- Add an entry to CHANGELOG.md for user-visible changes
