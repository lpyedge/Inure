#!/usr/bin/env python3
"""server.py [port]: static site + event/code/submit endpoints for the bench bench (127.0.0.1 only).

Static files come only from eval/bench/site/. Events, codes and expected data live outside that directory.
  POST /event?arm=<EP>&v=<text>   append an operator-visible page event to events/<EP>.jsonl
  GET  /code?arm=<EP>             confirmation code of this episode (created on first request, random)
  POST /submit?arm=<EP>           JSON body; records a submit event and returns {"ok":true,"code":...}
"""
import http.server
import json
import os
import re
import secrets
import socketserver
import sys
import threading
import time
import urllib.parse

HERE = os.path.dirname(os.path.abspath(__file__))
SITE = os.path.join(HERE, "site")
EVENTS = os.path.join(HERE, "events")
ARM_RE = re.compile(r"[A-Za-z0-9_-]{1,64}")
CODE_ALPHABET = "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789"
MAX_V = 500
MAX_BODY = 16384
_lock = threading.Lock()
_codes = {}


def get_code(arm):
    """Per-episode confirmation code: memory first, then the .code file, else a new random code."""
    with _lock:
        if arm in _codes:
            return _codes[arm]
        path = os.path.join(EVENTS, f"{arm}.code")
        if os.path.exists(path):
            with open(path, encoding="utf-8") as f:
                code = f.read().strip()
        else:
            code = "".join(secrets.choice(CODE_ALPHABET) for _ in range(7))
            with open(path, "w", encoding="utf-8") as f:
                f.write(code + "\n")
        _codes[arm] = code
        return code


def append_event(arm, rec):
    rec["t"] = time.time()
    line = json.dumps(rec, ensure_ascii=False) + "\n"
    with _lock:
        with open(os.path.join(EVENTS, f"{arm}.jsonl"), "a", encoding="utf-8") as f:
            f.write(line)


class Handler(http.server.SimpleHTTPRequestHandler):
    def __init__(self, *a, **k):
        super().__init__(*a, directory=SITE, **k)

    def log_message(self, *a):
        pass

    def _json(self, status, obj):
        body = json.dumps(obj, ensure_ascii=False).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Cache-Control", "no-store")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _arm(self, q):
        arm = q.get("arm", [""])[0]
        return arm if ARM_RE.fullmatch(arm) else None

    def do_GET(self):
        q = urllib.parse.parse_qs(urllib.parse.urlparse(self.path).query)
        path = urllib.parse.urlparse(self.path).path
        if path == "/code":
            arm = self._arm(q)
            if arm is None:
                return self._json(400, {"error": "bad arm"})
            return self._json(200, {"code": get_code(arm)})
        return super().do_GET()

    def do_POST(self):
        parsed = urllib.parse.urlparse(self.path)
        q = urllib.parse.parse_qs(parsed.query)
        arm = self._arm(q)
        if arm is None:
            return self._json(400, {"error": "bad arm"})
        if parsed.path == "/event":
            v = q.get("v", [""])[0][:MAX_V]
            append_event(arm, {"kind": "event", "v": v})
            self.send_response(204)
            self.end_headers()
            return
        if parsed.path == "/submit":
            try:
                n = int(self.headers.get("Content-Length", "0"))
            except ValueError:
                n = -1
            if n < 0 or n > MAX_BODY:
                return self._json(400, {"error": "bad body length"})
            try:
                data = json.loads(self.rfile.read(n).decode("utf-8"))
            except ValueError:
                return self._json(400, {"error": "bad json"})
            if not isinstance(data, dict):
                return self._json(400, {"error": "body must be an object"})
            append_event(arm, {"kind": "submit", "v": "submit", "data": data})
            return self._json(200, {"ok": True, "code": get_code(arm)})
        return self._json(404, {"error": "not found"})


class Server(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 18295
    os.makedirs(EVENTS, exist_ok=True)
    with Server(("127.0.0.1", port), Handler) as srv:
        srv.serve_forever()
