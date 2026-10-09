---
created: 2026-10-08
updated: 2026-10-08
---

# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/).

## [0.2.0] - 2026-10-08

### Added
- Automatic failover: transport errors, timeouts and HTTP 408/429/5xx are retried on another healthy backend (`proxy.max_retries`).
- `proxy.fanout_methods`: broadcast a call (typically `sendTransaction`) to every healthy backend, first success wins, upstream error surfaced otherwise.
- `proxy.blocked_methods`: reject methods before they reach a backend (403 + JSON-RPC error), including inside batches.
- API key via `x-api-key` header or `Authorization: Bearer`, in addition to `?api-key=`.
- `x-request-id` generation/propagation; `x-rpc-backend` and `x-rpc-attempts` response headers.
- Batch (array) JSON-RPC requests are recognised for routing and metrics.
- `/health` reports per-backend slot, probe latency, unix timestamps, healthy/total counts and the router version; returns 503 when no backend is healthy.
- Metrics: `rpc_upstream_attempts_total`, `rpc_failovers_total`, `rpc_fanout_total`, `rpc_blocked_total`, `rpc_backend_slot`, `rpc_backend_slot_lag`, `rpc_backend_health_check_duration_seconds`.
- Grafana dashboard rows for backends, resilience and per-owner usage.
- Graceful shutdown on SIGINT/SIGTERM with `proxy.shutdown_grace_secs`.
- `REDIS_URL` and `RPC_ROUTER_CONFIG` environment variables; `--version` flags.
- `rpc-admin`: `delete` command, `--expires-in` relative expiry, `--owner` filter on `list`, status/expiry columns, live per-second usage in `inspect`.
- Dockerfile, `docker-compose.yml` (router + Redis + Prometheus + Grafana), CI matrix with clippy/fmt/Redis integration/docker build, release workflow publishing binaries and a ghcr.io image.
- Config validation for backend URL schemes and fan-out/blocked conflicts.

### Changed
- All router-generated error bodies are JSON-RPC shaped and carry the request `id`; 429 responses include `Retry-After`.
- `rpc_method` metric label is bounded to known Solana methods (`other` otherwise) to prevent cardinality blow-up.
- Backend URLs that carry their own query string are merged correctly with client sub-paths and query parameters.
- Router credentials and hop-by-hop headers are stripped before forwarding upstream; `set-cookie` and hop-by-hop headers from upstream responses are stripped before reaching clients.
- Key metadata is fetched with a single `HGETALL`; the rate-limit script is compiled once.
- `Cargo.lock` is committed; release profile enables LTO and symbol stripping.

### Fixed
- `expires_at` set by `rpc-admin` was stored but never enforced; expired keys are now rejected.
- WebSocket backend connects are bounded by a 10 s timeout instead of hanging the upgrade.

## [0.1.0]

Initial release: API key auth, Redis rate limiting, weighted load balancing, method routing, WebSocket proxying, consensus health checks, Prometheus metrics, hot reload, Grafana dashboard.
