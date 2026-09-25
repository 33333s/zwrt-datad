#!/usr/bin/env python3
"""Offline HTTP contract test for the firmware-derived U50 candidate."""
import json
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
import socket
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

BINARY = sys.argv[1] if len(sys.argv) > 1 else "rust/target/debug/zwrt-datad"

class Handler(BaseHTTPRequestHandler):
    reply = b'{"model_name":"U50Pro","wa_inner_version":"B02","network_type":"NR5G","network_provider_fullname":"Test","battery_value":"84","sim_iccid":"private"}'
    query = None
    def do_GET(self):
        path = urlparse(self.path)
        Handler.query = (path.path, parse_qs(path.query))
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(Handler.reply)
    def log_message(self, *_args):
        pass

server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
thread = threading.Thread(target=server.serve_forever, daemon=True)
thread.start()
url = f"http://127.0.0.1:{server.server_port}/goform/goform_get_cmd_process"
try:
    result = subprocess.run([BINARY, "--u50-model", "u50pro", "--u50-goform-url", url, "--once"], capture_output=True, text=True, timeout=15, check=True)
    state = json.loads(result.stdout)
    assert state["device"]["api_template"] == "U50PRO"
    assert state["device"]["api_template_supported"] == 0
    assert state["net"]["type"] == "NR5G"
    assert "sim_iccid" not in result.stdout
    assert Handler.query[0] == "/goform/goform_get_cmd_process"
    assert "model_name" in Handler.query[1]["cmd"][0]
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        port = probe.getsockname()[1]
    process = subprocess.Popen([BINARY, "--u50-model", "u50pro", "--u50-goform-url", url, "--port", str(port)], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
    try:
        for _ in range(40):
            try:
                with urllib.request.urlopen(f"http://127.0.0.1:{port}/state", timeout=1) as response:
                    assert json.load(response)["device"]["api_template_supported"] == 0
                break
            except urllib.error.URLError:
                time.sleep(0.1)
        else:
            raise AssertionError("candidate server did not start")
        with urllib.request.urlopen(f"http://127.0.0.1:{port}/capabilities", timeout=2) as response:
            assert json.load(response)["controls"] == []
        try:
            urllib.request.urlopen(f"http://127.0.0.1:{port}/control", timeout=2)
            raise AssertionError("control route was exposed")
        except urllib.error.HTTPError as error:
            assert error.code == 404
    finally:
        process.terminate()
        process.wait(timeout=5)
    Handler.reply = b"<html>login required</html>"
    result = subprocess.run([BINARY, "--u50-model", "u50s", "--u50-goform-url", url, "--once"], capture_output=True, text=True, timeout=15)
    assert result.returncode != 0
    print("U50 candidate HTTP probe OK")
finally:
    server.shutdown()
    server.server_close()
