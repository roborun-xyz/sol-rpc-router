# sol-rpc-router

A high-performance reverse-proxy for Solana JSON-RPC and WebSocket endpoints with Redis-backed API key authentication, per-key rate limiting, weighted load balancing, method-based routing, and automatic health checks.

## Features

- **API Key Authentication**: query parameter `?api-key=` validated against Redis with local caching (moka, 60 s TTL).
- **Rate Limiting**: per-key RPS limits enforced atomically in Redis (INCR + EXPIRE Lua script).
- **Weighted Load Balancing**: distribute requests across backends by configurable weight; unhealthy backends are automatically excluded.
- **Method-Based Routing**: pin specific RPC methods (e.g. `getSlot`) to designated backends.
- **WebSocket Proxying**: separate WS server (HTTP port + 1) with the same auth, rate limiting, and weighted backend selection.
- **Health Checks**: background loop calls a configurable RPC method per backend; consecutive-failure / consecutive-success thresholds control status transitions.
- **Prometheus Metrics**: `GET /metrics` exposes request counts, latencies, and backend health gauges.
- **Admin CLI** (`rpc-admin`): create, list, inspect, and revoke API keys in Redis.

## Prerequisites

- Rust 2021 edition (stable)
- Redis (for API key storage and rate limiting)

## Quick Start (Local)

```bash
# Build
cargo build --release

# Run the router (requires Redis running)
./target/release/sol-rpc-router --config config.toml

# Create an API key
./target/release/rpc-admin create my-client --rate-limit 50
```

## Quick Start (Docker)

Run the router and Redis stack with a single command:

```bash
# 1. Start the stack (uses config.docker.toml)
docker compose up -d

# 2. Generate an API key inside the container
docker compose exec sol-rpc-router rpc-admin create my-client --rate-limit 100 --redis-url redis://redis:6379

# 3. Test it
curl "http://localhost:8080/?api-key=<YOUR_KEY>" -X POST -H "Content-Type: application/json" -d '{"jsonrpc":"2.0","id":1,"method":"getSlot"}'
```

To customize the configuration, edit `config.docker.toml` and hot-reload without downtime:
```bash
docker compose kill -s SIGHUP sol-rpc-router
```

Or restart the container to pick up changes:
```bash
docker compose restart sol-rpc-router
```

## Configuration

The router reads a TOML file (default `config.toml`).

```toml
port = 28899                          # HTTP; WebSocket listens on 28900
redis_url = "redis://127.0.0.1:6379/0"

[[backends]]
label = "mainnet-primary"
url = "https://api.mainnet-beta.solana.com"
weight = 10
ws_url = "wss://api.mainnet-beta.solana.com"   # optional

[[backends]]
label = "backup-rpc"
url = "https://solana-api.com"
weight = 5

[proxy]
timeout_secs = 30                     # upstream request timeout

[health_check]
interval_secs = 30                    # check frequency
timeout_secs = 5                      # per-check timeout
method = "getSlot"                    # RPC method used for probes
consecutive_failures_threshold = 3    # failures before marking unhealthy
consecutive_successes_threshold = 2   # successes before marking healthy

[method_routes]                       # optional per-method overrides
getSlot = "mainnet-primary"
```

### Config Validation

`load_config()` enforces:

- `redis_url` must be non-empty.
- At least one backend required; labels must be unique and non-empty.
- Backend weights must be > 0.
- `proxy.timeout_secs` must be > 0.
- `method_routes` values must reference existing backend labels.

## API Key Management CLI

```bash
# Create an API key (auto-generated)
rpc-admin create <owner> --rate-limit 10

# Create with a specific key value
rpc-admin create <owner> --rate-limit 10 --key my-custom-key

# List all keys
rpc-admin list

# Inspect a key
rpc-admin inspect <api_key>

# Revoke a key
rpc-admin revoke <api_key>

# Update a key
rpc-admin update <api_key> --rate-limit 100 --active true
```

Redis URL can be set via `--redis-url` flag or `REDIS_URL` env var (default `redis://127.0.0.1:6379`).

## Endpoints

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/` | POST | Proxy JSON-RPC requests (requires `?api-key=`) |
| `/*path` | POST | Proxy with subpath |
| `/health` | GET | Backend health status (JSON) |
| `/metrics` | GET | Prometheus metrics |
| `ws://host:port+1/` | WS | WebSocket proxy (requires `?api-key=`) |

## Testing

```bash
cargo test               # run all 35 tests
cargo test -- --list     # list test names
```

All tests use mocks only -- no Redis or real HTTP backends required (except localhost mock servers started in-process).
