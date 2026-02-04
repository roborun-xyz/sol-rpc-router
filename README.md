# RPC Router

A high-performance HTTP router for Solana RPC requests with API key authentication, weighted load balancing, and method-based routing.

## Features

- **API Key Authentication**: Validates requests using query parameter `?api-key=`
- **Weighted Load Balancing**: Distribute requests across multiple backends with configurable weights
- **Method-Based Routing**: Route specific RPC methods to designated backends
- **Health Checks**: Automatically monitor backend health and route around unhealthy backends
- **Request Logging**: Logs request information including RPC method, path, client IP, and duration
- **Health Monitoring**: GET /health endpoint for external monitoring tools

## Configuration

The router uses a TOML configuration file specified via command-line argument.

### TOML Configuration

1. Copy the example configuration:

   ```bash
   cp config.example.toml config.toml
   ```

2. Edit `config.toml` with your settings:

   ```toml
   # Server port (HTTP JSON-RPC)
   # WebSocket server automatically runs on port + 1 (Solana convention)
   port = 28899

   # API keys for authentication
   api_keys = ["key1", "key2", "key3"]

   # Backend RPC endpoints with weights
   [[backends]]
   label = "backend-0"
   url = "https://api.mainnet-beta.solana.com"
   weight = 2

   [[backends]]
   label = "backend-1"
   url = "https://solana-api.com"
   weight = 1

   # Health check configuration (optional - has defaults)
   [health_check]
   interval_secs = 30              # Check every 30 seconds
   timeout_secs = 5                # 5 second timeout
   method = "getSlot"              # RPC method for health checks
   consecutive_failures_threshold = 3     # Mark unhealthy after 3 failures
   consecutive_successes_threshold = 2    # Mark healthy after 2 successes

   # Method-specific routing overrides (optional)
   # Use backend labels to route specific methods
   [method_routes]
   getProgramAccountsV2 = "backend-0"
   sendTransaction = "backend-1"
   ```

### Weighted Load Balancing

Backends are selected randomly based on their configured weights:

- **Weight 2**: Gets 2x more requests than weight 1
- **Weight 3**: Gets 3x more requests than weight 1
- **Example**: Weights [2, 3, 1] result in distribution [33.3%, 50%, 16.7%]

### Method-Based Routing

Override the weighted selection for specific RPC methods:

- Define method → backend label mappings in `[method_routes]`
- Use backend labels to reference backends
- **Method names are case-sensitive** - must match exactly what's in the JSON-RPC `"method"` field
- Useful for routing expensive operations to specific providers

### Health Checks

The router automatically monitors backend health:

- **Periodic Checks**: Sends health check requests to all backends at configured intervals (default: 30s)
- **Smart Routing**: Automatically excludes unhealthy backends from request routing
- **Thresholds**: Backends are marked unhealthy after consecutive failures (default: 3) and healthy after consecutive successes (default: 2)
- **Fallback Behavior**: Returns 503 Service Unavailable when all backends are unhealthy
- **Configurable Method**: Uses `getSlot` by default (universally supported across Solana RPC providers)

Health check configuration is optional. All fields have sensible defaults.

## Usage

1. Configure the router (see Configuration section above)

2. Run the router with default config file (`config.toml`):

   ```bash
   cargo run --release
   ```

3. Or specify a custom config file:

   ```bash
   cargo run --release -- --config /path/to/custom-config.toml
   # or short form:
   cargo run --release -- -c /path/to/custom-config.toml
   ```

4. Make requests with your API key:

   ```bash
   curl -X POST -H "Content-Type: application/json" \
     -d '{"jsonrpc":"2.0","id":1,"method":"getEpochInfo"}' \
     "http://localhost:28899?api-key=your-api-key"
   ```

5. Use with Solana CLI:
   ```bash
   solana -u "http://localhost:28899?api-key=your-api-key" epoch-info
   ```

## Health Monitoring

The router exposes a GET `/health` endpoint for monitoring backend status:

```bash
curl http://localhost:28899/health | jq
```

Example response:
```json
{
  "overall_status": "healthy",
  "backends": [
    {
      "label": "backend-0",
      "url": "https://api.mainnet-beta.solana.com",
      "healthy": true,
      "last_check": "SystemTime { tv_sec: 1234567890, tv_nsec: 123456789 }",
      "consecutive_failures": 0,
      "consecutive_successes": 5,
      "last_error": null
    },
    {
      "label": "backend-1",
      "url": "https://solana-api.com",
      "healthy": false,
      "last_check": "SystemTime { tv_sec: 1234567890, tv_nsec: 987654321 }",
      "consecutive_failures": 3,
      "consecutive_successes": 0,
      "last_error": "Health check timed out after 5s"
    }
  ]
}
```

The `/health` endpoint:
- Does not require API key authentication
- Returns `overall_status` of "healthy" if any backend is healthy, "unhealthy" if all are unhealthy
- Provides detailed status for each backend including failure counts and last error message
- Can be integrated with monitoring tools like Prometheus, Datadog, or simple uptime monitors
