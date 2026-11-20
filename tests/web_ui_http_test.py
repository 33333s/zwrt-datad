#!/usr/bin/env python3
"""Optional UI routes must never change existing API authentication or the root banner."""
import gzip
import http.client
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time

binary = str(Path(sys.argv[1]).resolve())
root = Path(__file__).resolve().parents[1]
token = "web-http-fixture-not-a-device-secret"


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def request(port, path, headers=None):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
    try:
        connection.request("GET", path, headers=headers or {})
        response = connection.getresponse()
        return response.status, dict(response.getheaders()), response.read()
    finally:
        connection.close()


with tempfile.TemporaryDirectory(prefix="datad-web-http-") as temporary:
    directory = Path(temporary)
    secret = directory / "token"
    secret.write_text(token)
    secret.chmod(0o600)
    environment = dict(os.environ, ZWRT_DATAD_DIR=temporary, ZWRT_DATAD_UBUS_BIN="/usr/bin/false",
                       ZWRT_DATAD_UCI_BIN="/usr/bin/false", ZWRT_DATAD_OTA_DISABLE_AUTO="1")
    for enabled in (True, False):
        local, lan = free_port(), free_port()
        command = [binary, "-p", str(local), "--lan-bind", "127.0.0.1", "--lan-port", str(lan),
                   "--auth-token-file", str(secret)]
        if not enabled:
            command.append("--disable-web-ui")
        process = subprocess.Popen(command, env=environment, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            deadline = time.monotonic() + 15
            while True:
                try:
                    if request(lan, "/healthz")[0] == 200:
                        break
                except OSError:
                    pass
                assert process.poll() is None and time.monotonic() < deadline, "startup failed"
                time.sleep(.1)
            assert request(lan, "/")[2] == b"zwrt-datad Rust rewrite\n"
            assert request(local, "/ui/")[0] != 200, "UI appeared on the unauthenticated listener"
            for route in ("/state", "/capabilities", "/efs/status", "/signal/stream"):
                assert request(lan, route)[0] == 401, route
            assert request(lan, "/state", {"Authorization": "Bearer " + token})[0] == 200
            if not enabled:
                assert request(lan, "/ui/")[0] != 200
                continue
            status, headers, _ = request(lan, "/ui")
            assert status == 307 and headers["location"] == "ui/", headers
            manifest = json.loads((root / "web/dist/asset-manifest.json").read_text())
            for route, file, content_type in manifest:
                status, headers, body = request(lan, route)
                assert status == 200 and headers["content-type"] == content_type, (route, status)
                assert headers["cache-control"] == "no-store"
                assert headers["x-content-type-options"] == "nosniff"
                assert "script-src 'self'" in headers["content-security-policy"]
                assert gzip.decompress(body) == gzip.decompress((root / "web/dist" / file).read_bytes())
            for route in ("/ui/core.js", "/ui/src/main.jsx", "/ui/../version.json", "/ui/%2e%2e/etc/passwd"):
                assert request(lan, route)[0] != 200, route
        finally:
            process.terminate()
            process.wait(timeout=10)
    # Default-on never creates a new LAN listener or a public loopback UI.
    for extra in ([], ["--disable-web-ui"]):
        local = free_port()
        process = subprocess.Popen([binary, "-p", str(local), *extra], env=environment,
                                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            deadline = time.monotonic() + 15
            while True:
                try:
                    if request(local, "/healthz")[0] == 200:
                        break
                except OSError:
                    pass
                assert process.poll() is None and time.monotonic() < deadline, "legacy startup failed"
                time.sleep(.1)
            assert request(local, "/ui/")[0] != 200
        finally:
            process.terminate(); process.wait(timeout=10)
print("Web UI HTTP checks passed: default-on, explicit disable, legacy loopback startup, fixed assets, gzip, proxy redirect and API auth.")
