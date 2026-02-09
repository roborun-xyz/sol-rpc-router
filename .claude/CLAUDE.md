# CLAUDE.md

## Project Overview

sol-rpc-router is a Solana JSON-RPC reverse proxy written in Rust. It sits in front of one or more Solana RPC backends and provides API key authentication, per-key rate limiting, weighted load balancing, method-based routing, WebSocket proxying, health checking, and Prometheus metrics.

## Build & Test

```bash
cargo build                # debug build
cargo build --release      # release build
cargo test                 # run all tests (no external deps needed)
cargo fmt                  # format code
cargo clippy               # lint
```

Tests use `MockKeyStore` (no Redis required). Mock HTTP backends bind to `127.0.0.1:0` in-process.

## Project Structure

```
src/
  main.rs           Entry point: CLI args, server setup, spawns health check loop
  config.rs         TOML config structs + load_config() with validation
  state.rs          AppState struct, select_backend() / select_ws_backend() (weighted random)
  handlers.rs       Axum handlers: proxy, ws_proxy, health_endpoint
                    Middleware: extract_rpc_method, log_requests, track_metrics
  health.rs         HealthState (RwLock<HashMap>), BackendHealthStatus, health_check_loop
  keystore.rs       KeyStore trait + RedisKeyStore (Redis + moka cache)
  mock.rs           MockKeyStore for testing (supports error injection via set_error())
  lib.rs            Module declarations
  bin/rpc-admin.rs  Admin CLI for API key CRUD operations
  bin/benchmark.rs  In-process benchmark for performance validation

tests/
  config_test.rs    Config validation paths
  handler_test.rs   Proxy errors, health endpoint, extract_rpc_method middleware
  keystore_test.rs  MockKeyStore behavior
  routing_test.rs   Backend selection (HTTP + WebSocket, healthy/unhealthy)
```

## Key Patterns

- **State**: `AppState` is shared via `Arc<AppState>` and passed to handlers via Axum's `State` extractor.
- **KeyStore trait**: `async fn validate_key(&self, key: &str) -> Result<Option<KeyInfo>, String>`. Returns `Ok(Some(info))` for valid, `Ok(None)` for invalid/inactive, `Err(msg)` for errors (including "Rate limit exceeded").
- **Health**: `HealthState` uses `RwLock<HashMap<String, BackendHealthStatus>>` for aggregate status. Individual `BackendConfig` structs use `Arc<AtomicBool>` for lock-free health checks on the hot path. Backends default to healthy. The health check loop runs in a background tokio task.
- **Backend selection**: Weighted random among healthy backends. Method routes override this if the target backend is healthy.
- **WebSocket**: Separate server on port+1. Same auth flow, then `select_ws_backend()` picks a backend with `ws_url` configured.
- **Tests**: Integration tests in `tests/` directory. Use `tower::ServiceExt::oneshot()` to test Axum routers without binding ports (except `start_mock_backend()` which binds to a random port for proxy tests).

## Code Conventions

- Async runtime: tokio
- HTTP client: hyper-util legacy Client with hyper-tls
- Framework: axum 0.7
- Error handling: `Result<T, Box<dyn std::error::Error>>` for config, `Result<T, String>` for keystore
- Logging: tracing crate
- Metrics: metrics crate + metrics-exporter-prometheus
