# RPC Proxy

A simple HTTP proxy for Solana RPC requests with API key authentication and request logging.

## Configuration

The proxy is configured using environment variables. Copy `.env.example` to `.env` and update the values:

```bash
cp .env.example .env
```

### Environment Variables

- `BACKEND_URL` - The backend Solana RPC server URL (no default, must be set)
- `API_KEYS` - Comma-separated list of API keys allowed for proxy access (no default, must be set)
- `PORT` - Port for the proxy server to listen on (default: `28899`)

## Usage

1. Set up your environment variables in `.env`
2. Run the proxy:
   ```bash
   cargo run
   ```

3. Make requests to the proxy with your API key:
   ```bash
   curl -X POST -H "Content-Type: application/json" \
     -d '{"jsonrpc":"2.0","id":1,"method":"getEpochInfo"}' \
     "http://localhost:28899?api-key=your-api-key"
   ```

4. Use with Solana CLI:
   ```bash
   solana -u "http://localhost:28899?api-key=your-api-key" epoch-info
   ```

## Features

- **API Key Authentication**: Validates requests using query parameter `?api-key=`
- **Request Logging**: Logs brief request information (method, path, client IP, duration)
- **Proxy Forwarding**: Forwards requests to backend Solana RPC server

