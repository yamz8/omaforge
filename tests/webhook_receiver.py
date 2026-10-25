"""Records webhook deliveries for the end-to-end tests.

    python3 tests/webhook_receiver.py <port> <log.jsonl> <secret>

Each POST is appended to the log with its event, delivery id, whether the signature is
valid for <secret>, and the parsed payload. Paths starting with /fail get a 500.
"""
import hashlib
import hmac
import http.server
import json
import sys

PORT, LOG, SECRET = int(sys.argv[1]), sys.argv[2], sys.argv[3].encode()


class Handler(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("content-length") or 0))
        expected = "sha256=" + hmac.new(SECRET, body, hashlib.sha256).hexdigest()
        record = {
            "path": self.path,
            "event": self.headers.get("x-github-event"),
            "delivery": self.headers.get("x-github-delivery"),
            "signature_ok": hmac.compare_digest(self.headers.get("x-hub-signature-256", ""), expected),
            "user_agent": self.headers.get("user-agent", ""),
            "payload": json.loads(body or b"null"),
        }
        with open(LOG, "a") as f:
            f.write(json.dumps(record) + "\n")
        status = 500 if self.path.startswith("/fail") else 200
        self.send_response(status)
        self.end_headers()
        self.wfile.write(b"thanks" if status == 200 else b"nope")

    def log_message(self, *args):
        pass


http.server.HTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
