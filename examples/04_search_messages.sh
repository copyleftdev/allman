#!/usr/bin/env bash
# Full-text search across all indexed messages.
set -euo pipefail

URL="http://localhost:8000/mcp"

echo "=== Searching for 'credential' ==="
curl -s "$URL" -H 'Content-Type: application/json' -d '{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "tools/call",
  "params": {
    "name": "search_messages",
    "arguments": { "query": "credential", "limit": 5 }
  }
}' | python3 -m json.tool
