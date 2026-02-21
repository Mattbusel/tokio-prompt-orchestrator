#!/usr/bin/env python3
"""
Drive the tokio-prompt-orchestrator MCP server over stdio JSON-RPC.
Sends 10 infer calls with varying prompt lengths, then pipeline_status.
Outputs JSON results to stdout.
"""

import subprocess
import json
import time
import sys

MCP_BIN = r"C:\Users\Matthew\Tokio Prompt\target\release\mcp.exe"

# 10 prompts with increasing lengths
PROMPTS = [
    "Hi",                                                          # ~2 chars
    "Explain recursion briefly.",                                   # ~28 chars
    "What is the difference between TCP and UDP protocols?",       # ~54 chars
    "Describe the key principles of object-oriented programming in software engineering.", # ~84 chars
    "Write a detailed comparison of REST and GraphQL APIs, covering their strengths, weaknesses, and ideal use cases for modern web applications.", # ~143 chars
    "Explain the concept of backpressure in reactive systems. How does it prevent resource exhaustion, and what strategies can be used to implement it effectively in a distributed pipeline architecture?", # ~200 chars
    " ".join(["The quick brown fox jumps over the lazy dog."] * 8),  # ~352 chars
    " ".join(["In the beginning was the Word, and the Word was with the pipeline, and the pipeline was good."] * 6), # ~570 chars
    " ".join(["Concurrent systems require careful orchestration of shared resources to avoid deadlocks, livelocks, and priority inversion scenarios that degrade throughput."] * 6), # ~990 chars
    " ".join(["Lorem ipsum dolor sit amet, consectetur adipiscing elit. Sed do eiusmod tempor incididunt ut labore et dolore magna aliqua. Ut enim ad minim veniam, quis nostrud exercitation ullamco laboris."] * 8), # ~1560 chars
]

def make_request(method, params, req_id):
    return json.dumps({
        "jsonrpc": "2.0",
        "id": req_id,
        "method": method,
        "params": params,
    }) + "\n"

def main():
    # Start MCP server
    proc = subprocess.Popen(
        [MCP_BIN, "--worker", "llama_cpp"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        bufsize=1,
    )

    results = []
    req_id = 1

    # Send initialize
    init_req = make_request("initialize", {
        "protocolVersion": "2024-11-05",
        "capabilities": {},
        "clientInfo": {"name": "bench", "version": "0.1.0"}
    }, req_id)
    proc.stdin.write(init_req)
    proc.stdin.flush()
    init_resp = proc.stdout.readline()
    req_id += 1

    # Send initialized notification
    notif = json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}) + "\n"
    proc.stdin.write(notif)
    proc.stdin.flush()
    time.sleep(0.2)

    # Run 10 infer calls
    for i, prompt in enumerate(PROMPTS):
        call_id = req_id
        req_id += 1

        infer_req = make_request("tools/call", {
            "name": "infer",
            "arguments": {
                "prompt": prompt,
                "session_id": f"bench-{i}",
            }
        }, call_id)

        start = time.perf_counter()
        proc.stdin.write(infer_req)
        proc.stdin.flush()
        resp_line = proc.stdout.readline()
        elapsed_ms = (time.perf_counter() - start) * 1000

        try:
            resp = json.loads(resp_line)
            # MCP tool results come in result.content[0].text
            content_text = resp.get("result", {}).get("content", [{}])[0].get("text", "{}")
            tool_result = json.loads(content_text)
        except (json.JSONDecodeError, IndexError, KeyError):
            tool_result = {"error": "parse_failed", "raw": resp_line.strip()}

        results.append({
            "call": i + 1,
            "prompt_len": len(prompt),
            "wall_clock_ms": round(elapsed_ms, 2),
            "server_latency_ms": tool_result.get("latency_ms"),
            "stage_latencies": tool_result.get("stage_latencies"),
            "deduped": tool_result.get("deduped"),
            "session_id": tool_result.get("session_id"),
        })

    # Call pipeline_status
    status_req = make_request("tools/call", {
        "name": "pipeline_status",
        "arguments": {}
    }, req_id)
    proc.stdin.write(status_req)
    proc.stdin.flush()
    status_line = proc.stdout.readline()

    try:
        status_resp = json.loads(status_line)
        status_text = status_resp.get("result", {}).get("content", [{}])[0].get("text", "{}")
        pipeline_status = json.loads(status_text)
    except (json.JSONDecodeError, IndexError, KeyError):
        pipeline_status = {"error": "parse_failed", "raw": status_line.strip()}

    # Terminate
    proc.stdin.close()
    proc.terminate()
    try:
        proc.wait(timeout=3)
    except subprocess.TimeoutExpired:
        proc.kill()

    # Output
    output = {
        "infer_results": results,
        "pipeline_status": pipeline_status,
    }
    print(json.dumps(output, indent=2))

if __name__ == "__main__":
    main()
