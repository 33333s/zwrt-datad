#!/usr/bin/env python3
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import sys

log = sys.argv[2]

class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("content-length", "0")))
        with open(log, "ab") as handle:
            handle.write(self.path.encode() + b"\t" + body + b"\n")
        payload = b'{"result":"success"}'
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, *_):
        pass

ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), Handler).serve_forever()
