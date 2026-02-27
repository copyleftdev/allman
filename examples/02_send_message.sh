#!/usr/bin/env bash
# Send a message from one agent to multiple recipients.
set -euo pipefail

URL="http://localhost:8000/mcp"

echo "=== Sending message ==="
curl -s "$URL" -H 'Content-Type: application/json' -d '{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "tools/call",
  "params": {
    "name": "send_message",
    "arguments": {
      "from_agent": "BlueTeamLead",
      "to": ["ForensicsAgent"],
      "subject": "Lateral movement detected",
      "body": "Credential dumping observed on WORKSTATION-47. Investigate immediately.",
      "project_id": ""
    }
  }
}' | python3 -m json.tool
