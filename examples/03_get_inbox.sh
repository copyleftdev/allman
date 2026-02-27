#!/usr/bin/env bash
# Drain an agent's inbox (destructive — messages removed after read).
set -euo pipefail

URL="http://localhost:8000/mcp"

echo "=== Checking ForensicsAgent inbox ==="
curl -s "$URL" -H 'Content-Type: application/json' -d '{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "tools/call",
  "params": {
    "name": "get_inbox",
    "arguments": { "agent_name": "ForensicsAgent" }
  }
}' | python3 -m json.tool
