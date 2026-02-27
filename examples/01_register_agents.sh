#!/usr/bin/env bash
# Register two agents under the same project.
set -euo pipefail

URL="http://localhost:8000/mcp"

echo "=== Registering BlueTeamLead ==="
curl -s "$URL" -H 'Content-Type: application/json' -d '{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "tools/call",
  "params": {
    "name": "create_agent",
    "arguments": {
      "project_key": "demo",
      "name_hint": "BlueTeamLead",
      "program": "security_ops",
      "model": "claude-sonnet"
    }
  }
}' | python3 -m json.tool

echo ""
echo "=== Registering ForensicsAgent ==="
curl -s "$URL" -H 'Content-Type: application/json' -d '{
  "jsonrpc": "2.0",
  "id": 2,
  "method": "tools/call",
  "params": {
    "name": "create_agent",
    "arguments": {
      "project_key": "demo",
      "name_hint": "ForensicsAgent",
      "program": "forensics",
      "model": "claude-sonnet"
    }
  }
}' | python3 -m json.tool
