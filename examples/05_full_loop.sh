#!/usr/bin/env bash
# End-to-end: register agents, send a message, drain inbox, search.
# Run with: bash examples/05_full_loop.sh
set -euo pipefail

URL="http://localhost:8000/mcp"

call() { curl -sf "$URL" -H 'Content-Type: application/json' -d "$1"; }

echo "=== 1. Register agents ==="
call '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"create_agent","arguments":{"project_key":"demo","name_hint":"Alice","program":"analyst"}}}' | python3 -m json.tool
call '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"create_agent","arguments":{"project_key":"demo","name_hint":"Bob","program":"responder"}}}' | python3 -m json.tool

echo ""
echo "=== 2. Alice sends to Bob ==="
call '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"send_message","arguments":{"from_agent":"Alice","to":["Bob"],"subject":"Anomaly in sector 4","body":"Traffic spike at 14:32 UTC. Possible exfiltration. Please investigate."}}}' | python3 -m json.tool

echo ""
echo "=== 3. Bob checks inbox ==="
call '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"get_inbox","arguments":{"agent_name":"Bob"}}}' | python3 -m json.tool

echo ""
echo "=== 4. Bob checks again (should be empty — inbox drains) ==="
call '{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"get_inbox","arguments":{"agent_name":"Bob"}}}' | python3 -m json.tool

echo ""
echo "=== 5. Search (wait for Tantivy index) ==="
sleep 0.3
call '{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"search_messages","arguments":{"query":"exfiltration","limit":3}}}' | python3 -m json.tool
