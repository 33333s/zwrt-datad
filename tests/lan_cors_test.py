#!/usr/bin/env python3
"""Browser access to the authenticated LAN listener: CORS, preflight and SSE."""
import http.client
import os
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

BIN = sys.argv[1] if len(sys.argv) > 1 else "rust/target/debug/zwrt-datad"
TOKEN = "lan-cors-fixture-token-0123456789abcdef"
LAN_ORIGIN = "http://192.168.0.2:2333"


def free_ports():
    sockets = [socket.socket(), socket.socket()]
    try:
        for sock in sockets:
            sock.bind(("127.0.0.1", 0))
        return [sock.getsockname()[1] for sock in sockets]
    finally:
        for sock in sockets:
            sock.close()


def request(port, method, path, headers=None, read=True):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
    connection.request(method, path, headers=headers or {})
    response = connection.getresponse()
    body = response.read() if read else b""
    return connection, response, body


with tempfile.TemporaryDirectory(prefix="datad-lan-cors-") as folder:
    token_file = Path(folder) / "auth.token"
    token_file.write_text(TOKEN)
    token_file.chmod(0o600)
    local_port, lan_port = free_ports()
    env = dict(os.environ, ZWRT_DATAD_DIR=folder, ZWRT_DATAD_UBUS_BIN="/usr/bin/false",
               ZWRT_DATAD_UCI_BIN="/usr/bin/false", ZWRT_DATAD_OTA_DISABLE_AUTO="1")
    process = subprocess.Popen(
        [BIN, "-i", "200", "-p", str(local_port), "--lan-bind", "127.0.0.1",
         "--lan-port", str(lan_port), "--auth-token-file", str(token_file)],
        env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        deadline = time.monotonic() + 8
        while True:
            try:
                request(lan_port, "GET", "/healthz")[0].close()
                break
            except OSError:
                if process.poll() is not None or time.monotonic() >= deadline:
                    raise AssertionError("datad did not start")
                time.sleep(0.1)

        # A browser preflight carries no token and must still be answered.
        connection, response, _ = request(lan_port, "OPTIONS", "/state", {
            "Origin": LAN_ORIGIN, "Access-Control-Request-Method": "GET",
            "Access-Control-Request-Headers": "authorization",
            "Access-Control-Request-Private-Network": "true"})
        assert response.status == 204, response.status
        assert response.getheader("Access-Control-Allow-Origin") == LAN_ORIGIN
        assert "authorization" in response.getheader("Access-Control-Allow-Headers")
        assert "GET" in response.getheader("Access-Control-Allow-Methods")
        assert response.getheader("Access-Control-Allow-Private-Network") == "true"
        connection.close()

        # Errors must be readable by the page too (otherwise a 401 looks like a
        # network failure).
        connection, response, _ = request(lan_port, "GET", "/state", {"Origin": LAN_ORIGIN})
        assert response.status == 401
        assert response.getheader("Access-Control-Allow-Origin") == LAN_ORIGIN
        connection.close()

        # Authenticated HTTP and the SSE stream both carry the grant.
        auth = {"Origin": LAN_ORIGIN, "Authorization": f"Bearer {TOKEN}"}
        connection, response, body = request(lan_port, "GET", "/state", auth)
        assert response.status == 200 and body.startswith(b"{")
        assert response.getheader("Access-Control-Allow-Origin") == LAN_ORIGIN
        connection.close()
        connection, response, _ = request(
            lan_port, "GET", f"/events?access_token={TOKEN}", {"Origin": LAN_ORIGIN}, read=False)
        assert response.status == 200
        assert response.getheader("Content-Type").startswith("text/event-stream")
        assert response.getheader("Access-Control-Allow-Origin") == LAN_ORIGIN
        assert response.readline().startswith(b"event: state"), "no SSE frame"
        connection.close()

        # Non-LAN origins get no grant, and their preflight gets no bypass.
        for origin in ("https://evil.example.com", "http://8.8.8.8", "null",
                       "http://192.168.0.2:2333/path"):
            connection, response, _ = request(lan_port, "OPTIONS", "/auth/login", {
                "Origin": origin, "Access-Control-Request-Method": "POST",
                "Access-Control-Request-Headers": "authorization"})
            assert response.getheader("Access-Control-Allow-Origin") is None, origin
            assert response.status != 204, origin
            connection.close()
        connection, response, _ = request(lan_port, "GET", "/healthz", {"Origin": "https://evil.example.com"})
        assert response.status == 200 and response.getheader("Access-Control-Allow-Origin") is None
        connection.close()

        # The loopback listener is unchanged: no CORS headers are added there.
        connection, response, _ = request(local_port, "GET", "/healthz", {"Origin": LAN_ORIGIN})
        assert response.getheader("Access-Control-Allow-Origin") is None
        connection.close()
    finally:
        process.terminate()
        process.wait(timeout=5)
print("LAN CORS, preflight and SSE OK")
