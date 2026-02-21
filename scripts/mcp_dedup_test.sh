#!/usr/bin/env bash
# mcp_dedup_test.sh — Exercise batch_infer + pipeline_status via MCP stdio
#
# Sends JSON-RPC messages to the MCP server binary over stdin/stdout
# to validate deduplication behavior with 20 identical prompts.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
MCP_BIN="$SCRIPT_DIR/target/release/mcp.exe"

if [ ! -f "$MCP_BIN" ]; then
  echo "ERROR: MCP binary not found at $MCP_BIN"
  echo "Build it with: cargo build --features mcp --bin mcp --release"
  exit 1
fi

echo "=== MCP Deduplication Validation Test ==="
echo "Binary: $MCP_BIN"
echo ""

# Create a temporary file for output capture
OUTFILE=$(mktemp)
trap 'rm -f "$OUTFILE"' EXIT

# Generate the 20 identical prompts for batch_infer
PROMPTS='["What is the capital of France?","What is the capital of France?","What is the capital of France?","What is the capital of France?","What is the capital of France?","What is the capital of France?","What is the capital of France?","What is the capital of France?","What is the capital of France?","What is the capital of France?","What is the capital of France?","What is the capital of France?","What is the capital of France?","What is the capital of France?","What is the capital of France?","What is the capital of France?","What is the capital of France?","What is the capital of France?","What is the capital of France?","What is the capital of France?"]'

# Build JSON-RPC messages
# 1. Initialize
INIT='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"dedup-test","version":"1.0"}}}'

# 2. Initialized notification
INITIALIZED='{"jsonrpc":"2.0","method":"notifications/initialized"}'

# 3. batch_infer call
BATCH_INFER="{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"batch_infer\",\"arguments\":{\"prompts\":$PROMPTS}}}"

# 4. Wait, then pipeline_status
PIPELINE_STATUS='{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"pipeline_status","arguments":{}}}'

echo "Step 1: Initializing MCP server..."
echo "Step 2: Sending batch_infer with 20 identical prompts..."
echo "Step 3: Querying pipeline_status..."
echo ""

# Send all messages in sequence via stdin, capture stdout
# Use timeout to prevent hanging
{
  echo "$INIT"
  sleep 1
  echo "$INITIALIZED"
  sleep 0.5
  echo "$BATCH_INFER"
  sleep 3
  echo "$PIPELINE_STATUS"
  sleep 2
} | timeout 15 "$MCP_BIN" --worker echo > "$OUTFILE" 2>/dev/null || true

echo "=== Raw MCP Responses ==="
cat "$OUTFILE"
echo ""
echo ""

# Parse responses
echo "=== Parsed Results ==="

# Extract batch_infer response (id:2)
BATCH_RESULT=$(grep -o '"id":2[^}]*}' "$OUTFILE" 2>/dev/null || echo "not found")
echo "batch_infer response: $BATCH_RESULT"

# Extract pipeline_status response (id:3)
STATUS_LINE=$(grep '"id":3' "$OUTFILE" 2>/dev/null || echo "")
echo "pipeline_status response line found: $([ -n "$STATUS_LINE" ] && echo 'yes' || echo 'no')"

# Try to extract dedup_stats from the response
if [ -n "$STATUS_LINE" ]; then
  echo ""
  echo "Full pipeline_status response:"
  echo "$STATUS_LINE" | python3 -m json.tool 2>/dev/null || echo "$STATUS_LINE"
fi

echo ""
echo "=== Test Complete ==="
