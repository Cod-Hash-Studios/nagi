#!/usr/bin/env python3
import json
import sys


def send(payload):
    sys.stdout.write(json.dumps(payload, separators=(",", ":")) + "\n")
    sys.stdout.flush()


for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    if method == "initialize":
        send({
            "jsonrpc": "2.0",
            "id": message["id"],
            "result": {
                "protocolVersion": 1,
                "agentInfo": {"name": "fixture-acp", "version": "1.0"},
                "agentCapabilities": {
                    "loadSession": True,
                    "sessionCapabilities": {"resume": {}, "close": {}},
                },
                "authMethods": [],
            },
        })
    elif method == "session/new":
        send({"jsonrpc": "2.0", "id": message["id"], "result": {"sessionId": "acp-session-1"}})
    elif method == "session/resume":
        send({"jsonrpc": "2.0", "id": message["id"], "result": {}})
    elif method == "session/prompt":
        session_id = message["params"]["sessionId"]
        send({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": session_id,
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": "fixture output"},
                },
            },
        })
        send({
            "jsonrpc": "2.0",
            "id": "permission-1",
            "method": "session/request_permission",
            "params": {
                "sessionId": session_id,
                "toolCall": {"toolCallId": "tool-1", "title": "Write fixture.txt"},
                "options": [
                    {"optionId": "allow-once", "name": "Allow once", "kind": "allow_once"},
                    {"optionId": "reject-once", "name": "Reject", "kind": "reject_once"},
                ],
            },
        })
        prompt_id = message["id"]
    elif method == "session/cancel":
        send({"jsonrpc": "2.0", "id": prompt_id, "result": {"stopReason": "cancelled"}})
    elif message.get("id") == "permission-1":
        selected = message["result"]["outcome"]
        if selected.get("outcome") != "selected" or selected.get("optionId") != "allow-once":
            sys.exit(2)
        send({"jsonrpc": "2.0", "id": prompt_id, "result": {"stopReason": "end_turn"}})
