#!/usr/bin/env bash
# Sends SIGHUP to every running sol-rpc-router so it re-reads its config.
# In Docker: docker compose kill -s HUP router
set -euo pipefail

pids=$(pgrep -x sol-rpc-router || true)
if [ -z "$pids" ]; then
  echo "sol-rpc-router is not running"
  exit 1
fi

echo "$pids" | xargs kill -HUP
echo "Sent SIGHUP to sol-rpc-router (pid(s): $(echo "$pids" | tr '\n' ' '))"
