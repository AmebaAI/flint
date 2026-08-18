#!/usr/bin/env python3
import json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class RuntimeHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_GET(self):
        if self.path == "/ping":
            self.respond(200, {"status": "Healthy"})
        elif self.path == "/.well-known/agent-card.json":
            self.respond(
                200,
                {
                    "name": "Flint Runtime Fixture",
                    "description": "Deterministic local AgentCore runtime fixture",
                    "version": "0.1.0",
                },
            )
        else:
            self.respond(404, {"message": "not found"})

    def do_POST(self):
        if self.path != "/invocations":
            self.respond(404, {"message": "not found"})
            return
        length = int(self.headers.get("Content-Length", "0"))
        self.rfile.read(length)
        self.respond(200, {"result": {"kind": "fixture"}, "status": "completed"})

    def respond(self, status, payload):
        body = json.dumps(payload, separators=(",", ":")).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, format_string, *args):
        return


if __name__ == "__main__":
    ThreadingHTTPServer(("0.0.0.0", 8080), RuntimeHandler).serve_forever()
