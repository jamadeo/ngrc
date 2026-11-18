#!/usr/bin/env python3
"""Tiny real stdio MCP fixture. No third-party dependencies or network."""
import json
import os
import sys

calls = 0
for line in sys.stdin:
    request = json.loads(line)
    if "id" not in request:
        continue
    method = request["method"]
    if method == "initialize":
        result = {
            "protocolVersion": request["params"]["protocolVersion"],
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "fixture", "version": "1"},
            "instructions": "Fixture extension instructions: use probe to inspect the server.",
        }
    elif method == "tools/list":
        result = {"tools": [{"name": "probe", "description": "Inspect fixture", "inputSchema": {"type": "object", "properties": {}}}]}
    elif method == "tools/call":
        calls += 1
        result = {"content": [{"type": "text", "text": json.dumps({"calls": calls, "cwd": os.getcwd(), "env": os.environ.get("NGRC_FIXTURE")})}], "isError": False}
    elif method == "ping":
        result = {}
    else:
        print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "error": {"code": -32601, "message": "Unknown method"}}), flush=True)
        continue
    print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}), flush=True)
