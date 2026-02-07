# RPC Router

A high-performance HTTP router for Solana RPC requests with Redis-backed API key authentication, rate limiting, weighted load balancing, and method-based routing.

## Features

- **API Key Authentication**: Validates requests using query parameter `?api-key=` against Redis.
- **Rate Limiting**: Enforces per-key rate limits (requests per second) using Redis + local caching.
- **Weighted Load Balancing**: Distribute requests across multiple backends with configurable weights.
- **Method-Based Routing**: Route specific RPC methods to designated backends.
- **Health Checks**: Automatically monitor backend health and route around unhealthy backends.
- **Request Logging**: Logs request information including RPC method, path, client IP, and duration.
- **Health Monitoring**: GET /health endpoint for external monitoring tools.

## Prerequisites

- **Redis**: Required for storing API keys and rate limiting counters.

## Configuration

The router uses a TOML configuration file.

1. Create `config.toml`:

   ```toml
   # Server port (HTTP JSON-RPC)
   # WebSocket server automatically runs on port + 1
   port = 28899

   # Redis Connection URL
   redis_url = "redis://127.0.0.1:6379/0"

   # Backend RPC endpoints with weights
   [[backends]]
   label = "mainnet-beta"
   url = "https://api.mainnet-beta.solana.com"
   weight = 10

   [[backends]]
   label = "backup-rpc"
   url = "https://solana-api.com"
   weight = 5

   # Proxy settings
   [proxy]
   timeout_secs = 30

   # Health check configuration
   [health_check]
   interval_secs = 30
   timeout_secs = 5
   method = "getSlot"
   ```

## Key Management CLI

Use the built-in `rpc-admin` CLI to manage API keys.

**Build:**
```bash
cargo build --release --bin rpc-admin
```

**Commands:**

1. **Create a Key**:
   ```bash
   # Create key for client-a with 600 req/min limit
   ./target/release/rpc-admin create client-a --rate-limit 600
   ```

2. **List Keys**:
   ```bash
   ./target/release/rpc-admin list
   ```

3. **Get Key Info**:
   ```bash
   ./target/release/rpc-admin inspect <api_key>
   ```

4. **Revoke Key**:
   ```bash
   ./target/release/rpc-admin revoke <api_key>
   ```

**Redis Configuration:**
By default, `rpc-admin` connects to `redis://127.0.0.1:6379`.
To change this, set the `REDIS_URL` environment variable or use the `--redis-url` flag:

```bash
export REDIS_URL="redis://redis-host:6379"
./target/release/rpc-admin list
```

## Running the Router

1. Ensure Redis is running.
2. Run the server:

   ```bash
   cargo run --release -- --config config.toml
   ```

3. Make requests:

   ```bash
   curl -X POST -H "Content-Type: application/json" \
     -d '{"jsonrpc":"2.0","id":1,"method":"getSlot"}' \
     "http://localhost:28899?api-key=YOUR_API_KEY"
   ```

## Health Monitoring

The `/health` endpoint exposes backend status:

```bash
curl http://localhost:28899/health
```
