"""Runs on the box: python3 key_probe_server.py <port> <expected_sha256_file> <account_file> <log> <seconds>

A loopback "account API" for the keyed-fetch feature. GET /v1/me answers 200 {"account": <the run's random account
name>} only when the request's Authorization bearer token hashes to the expected sha256, and 401 otherwise. It never
echoes a header back. Each request appends one line to <log>: the path, the status and whether a token came, never
the token. Exits after <seconds>.
"""
import hashlib
import http.server
import json
import sys
import threading

port, expected_file, account_file, log, seconds = sys.argv[1:6]
expected = open(expected_file).read().strip()
account = open(account_file).read().strip()


class Probe(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_GET(self):
        auth = self.headers.get("authorization") or ""
        token = auth[len("Bearer "):] if auth.startswith("Bearer ") else ""
        good = bool(token) and hashlib.sha256(token.encode()).hexdigest() == expected
        status, body = (200, {"account": account}) if good and self.path == "/v1/me" else (401, {"error": "unauthorized"})
        with open(log, "a") as f:
            f.write(json.dumps({"path": self.path, "status": status, "bearer_token_sent": bool(token)}) + "\n")
        data = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.end_headers()
        self.wfile.write(data)


server = http.server.ThreadingHTTPServer(("127.0.0.1", int(port)), Probe)
threading.Timer(float(seconds), server.shutdown).start()
server.serve_forever()
