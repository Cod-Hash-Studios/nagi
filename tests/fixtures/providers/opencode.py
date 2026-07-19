#!/usr/bin/env python3

import json
import os
import queue
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlparse


TESTED_VERSION = "1.18.3"
EVENTS = queue.Queue()
SCENARIO = "disconnect" if "disconnect" in Path(sys.argv[0]).name else "conformance"
PROMPT_COUNT = 0


def event(event_type, properties, event_id=None):
    payload = {"type": event_type, "properties": properties}
    if event_id is not None:
        payload["id"] = event_id
    EVENTS.put(json.dumps(payload, separators=(",", ":")))


def required_doc():
    return {
        "paths": {
            "/event": {"get": {}},
            "/session": {"post": {}},
            "/session/status": {"get": {}},
            "/session/{sessionID}": {"get": {}},
            "/session/{sessionID}/message": {"get": {}},
            "/session/{sessionID}/prompt_async": {"post": {}},
            "/session/{sessionID}/abort": {"post": {}},
            "/permission": {"get": {}},
            "/permission/{requestID}/reply": {"post": {}},
            "/question": {"get": {}},
        }
    }


def complete_first_turn(reply):
    event(
        "permission.replied",
        {"sessionID": "session-live", "requestID": "attention-1", "reply": reply},
        "event-permission-replied",
    )
    event(
        "message.updated",
        {"info": {"id": "message-1", "sessionID": "session-live", "role": "assistant"}},
        "event-message",
    )
    event(
        "message.part.updated",
        {
            "sessionID": "session-live",
            "part": {
                "id": "part-1",
                "sessionID": "session-live",
                "messageID": "message-1",
                "type": "text",
                "text": "",
            },
            "time": 1,
        },
        "event-part-start",
    )
    event(
        "message.part.delta",
        {
            "sessionID": "session-live",
            "messageID": "message-1",
            "partID": "part-1",
            "field": "text",
            "delta": "done",
        },
        "event-part-delta",
    )
    event(
        "message.part.updated",
        {
            "sessionID": "session-live",
            "part": {
                "id": "part-1",
                "sessionID": "session-live",
                "messageID": "message-1",
                "type": "text",
                "text": "done",
            },
            "time": 2,
        },
        "event-part-end",
    )
    event(
        "session.status",
        {"sessionID": "session-live", "status": {"type": "idle"}},
        "event-first-idle",
    )


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, _format, *_arguments):
        pass

    def body(self):
        length = int(self.headers.get("Content-Length", "0"))
        return self.rfile.read(length) if length else b""

    def json_response(self, value, status=200):
        body = json.dumps(value, separators=(",", ":")).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(body)
        self.wfile.flush()

    def empty_response(self, status=204):
        self.send_response(status)
        self.send_header("Content-Length", "0")
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.flush()

    def do_GET(self):
        parsed = urlparse(self.path)
        path = parsed.path
        directory = parse_qs(parsed.query).get("directory", [""])[0]

        if path == "/global/health":
            self.json_response({"healthy": True, "version": TESTED_VERSION})
        elif path == "/doc":
            self.json_response(required_doc())
        elif path == "/event":
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Cache-Control", "no-cache")
            self.send_header("Connection", "close")
            self.end_headers()
            self.wfile.write(b'data: {"type":"server.connected","properties":{}}\n\n')
            self.wfile.flush()
            while True:
                try:
                    payload = EVENTS.get(timeout=15)
                except queue.Empty:
                    payload = '{"type":"server.heartbeat","properties":{}}'
                try:
                    self.wfile.write(f"data: {payload}\n\n".encode())
                    self.wfile.flush()
                except (BrokenPipeError, ConnectionResetError):
                    return
        elif path == "/session/status":
            self.json_response({"session-live": {"type": "idle"}, "session-resumed": {"type": "idle"}})
        elif path == "/permission" or path == "/question":
            self.json_response([])
        elif path.startswith("/session/") and path.endswith("/message"):
            self.json_response([])
        elif path.startswith("/session/"):
            session_id = path.removeprefix("/session/")
            self.json_response({"id": session_id, "directory": directory})
        else:
            self.json_response({"error": "not found"}, status=404)

    def do_POST(self):
        global PROMPT_COUNT

        parsed = urlparse(self.path)
        path = parsed.path
        body = self.body()

        if path == "/session":
            self.json_response({"id": "session-live"})
            return

        if path.endswith("/prompt_async"):
            PROMPT_COUNT += 1
            self.empty_response()
            session_id = path.split("/")[2]
            event(
                "message.updated",
                {
                    "info": {
                        "id": f"user-message-{PROMPT_COUNT}",
                        "sessionID": session_id,
                        "role": "user",
                    }
                },
                f"event-user-message-{PROMPT_COUNT}",
            )
            event(
                "session.status",
                {"sessionID": session_id, "status": {"type": "busy"}},
                f"event-busy-{PROMPT_COUNT}",
            )
            if SCENARIO == "disconnect":
                threading.Timer(0.1, lambda: os._exit(0)).start()
            elif PROMPT_COUNT == 1:
                event(
                    "permission.asked",
                    {
                        "id": "attention-1",
                        "sessionID": "session-live",
                        "permission": "bash",
                        "patterns": ["cargo test"],
                        "metadata": {"secret": "must-not-escape"},
                        "always": [],
                        "tool": {"messageID": "message-tool", "callID": "call-1"},
                    },
                    "event-permission",
                )
            return

        if path == "/permission/attention-1/reply":
            reply = json.loads(body or b"{}").get("reply", "once")
            self.json_response(True)
            complete_first_turn(reply)
            return

        if path.endswith("/abort"):
            self.json_response(True)
            session_id = path.split("/")[2]
            event(
                "session.status",
                {"sessionID": session_id, "status": {"type": "idle"}},
                "event-interrupt-idle",
            )
            return

        self.json_response({"error": "not found"}, status=404)


if len(sys.argv) > 1 and sys.argv[1] == "--version":
    print(TESTED_VERSION)
    raise SystemExit(0)

server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
server.daemon_threads = True
print(f"opencode server listening on http://127.0.0.1:{server.server_port}", flush=True)
server.serve_forever()
