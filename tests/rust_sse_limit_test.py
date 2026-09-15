#!/usr/bin/env python3
import http.client
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time
import urllib.request


def connect(port):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=3)
    connection.request("GET", "/events")
    return connection, connection.getresponse()


def main():
    binary = Path(sys.argv[1]).resolve()
    with socket.socket() as reserved:
        reserved.bind(("127.0.0.1", 0))
        port = reserved.getsockname()[1]
    clients = []
    with tempfile.TemporaryDirectory(prefix="datad-sse-test-") as folder:
        env = dict(os.environ, ZWRT_DATAD_DIR=folder,
                   ZWRT_DATAD_UBUS_BIN="/usr/bin/false",
                   ZWRT_DATAD_UCI_BIN="/usr/bin/false")
        process = subprocess.Popen(
            [str(binary), "-i", "200", "-p", str(port)], env=env,
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            deadline = time.monotonic() + 8
            while True:
                try:
                    urllib.request.urlopen(
                        f"http://127.0.0.1:{port}/healthz", timeout=1).close()
                    break
                except OSError:
                    if process.poll() is not None or time.monotonic() >= deadline:
                        raise AssertionError("datad did not start")
                    time.sleep(0.1)

            for _ in range(16):
                connection, response = connect(port)
                assert response.status == 200, response.status
                clients.append((connection, response))
            rejected, response = connect(port)
            assert response.status == 503, response.status
            response.read()
            rejected.close()

            clients.pop(0)[0].close()
            deadline = time.monotonic() + 8
            while True:
                replacement, response = connect(port)
                if response.status == 200:
                    replacement.close()
                    break
                response.read()
                replacement.close()
                assert response.status == 503, response.status
                if time.monotonic() >= deadline:
                    raise AssertionError("SSE slot was not released after disconnect")
                time.sleep(0.2)
        finally:
            for connection, _ in clients:
                connection.close()
            process.terminate()
            try:
                process.wait(timeout=8)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()


if __name__ == "__main__":
    main()
