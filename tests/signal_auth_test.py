#!/usr/bin/env python3
"""No modem access: actual HTTP auth boundary and native worker bind policy."""
import http.client
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parent.parent
BIN = str(Path(sys.argv[1]).resolve())
TOKEN = "signal-auth-test-not-a-real-device-secret"


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def request(port, path, headers=None):
    c = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
    try:
        c.request("GET", path, headers=headers or {})
        r = c.getresponse()
        return r.status, r.read()
    finally:
        c.close()


with tempfile.TemporaryDirectory(prefix="datad-signal-auth-") as tmp:
    directory = Path(tmp)
    token = directory / "auth.token"
    token.write_text(TOKEN)
    token.chmod(0o600)
    local_port, lan_port = free_port(), free_port()
    env = dict(os.environ, ZWRT_DATAD_DIR=tmp, ZWRT_DATAD_UBUS_BIN="/usr/bin/false",
               ZWRT_DATAD_UCI_BIN="/usr/bin/false", ZWRT_DATAD_OTA_DISABLE_AUTO="1")
    proc = subprocess.Popen([BIN, "-p", str(local_port), "--lan-bind", "127.0.0.1",
                             "--lan-port", str(lan_port), "--auth-token-file", str(token)],
                            env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        deadline = time.monotonic() + 10
        while True:
            try:
                if request(lan_port, "/healthz")[0] == 200:
                    break
            except OSError:
                pass
            assert proc.poll() is None and time.monotonic() < deadline, "datad startup failed"
            time.sleep(0.1)
        for port in (local_port, lan_port):
            for path, headers in [
                ("/signal/stream", {}),
                ("/signal/stream", {"Authorization": "Bearer wrong"}),
                ("/signal/stream", {"Authorization": "Basic " + TOKEN}),
                ("/signal/stream?access_token=" + TOKEN, {}),
                ("/signal/stream", {"X-Auth-Token": TOKEN}),
            ]:
                status, body = request(port, path, headers)
                assert status == 401, (port, path.split('?')[0], status)
                assert TOKEN.encode() not in body
            # No worker in this host fixture: valid auth reaches the controlled
            # unavailable response. Missing auth must never reach that stage.
            assert request(port, "/signal/stream", {"Authorization": "Bearer " + TOKEN})[0] == 503
    finally:
        proc.terminate()
        proc.wait(timeout=8)

    worker = directory / "streamer"
    subprocess.run(["cc", "-O2", str(ROOT / "rust/u50_diag_streamer.c"), "-ldl", "-o", str(worker)], check=True)
    heartbeat = directory / "heartbeat"
    heartbeat.touch()
    for bind in ("0.0.0.0", "192.168.0.1", "127.0.0.2"):
        status_path = directory / "status.json"
        p = subprocess.run([str(worker), str(status_path), str(heartbeat), str(free_port()), bind], timeout=5)
        assert p.returncode == 64
        assert json.loads(status_path.read_text())["reason"] == "loopback_required"
    # The approved bind reaches libdiag loading; this PC has no vendor library.
    p = subprocess.run([str(worker), str(status_path), str(heartbeat), str(free_port()), "127.0.0.1"], timeout=5)
    assert p.returncode == 66, p.returncode
    assert json.loads(status_path.read_text())["reason"] == "libdiag_missing"

print("Signal stream: missing/wrong/query auth denied, authorized request checked, LAN raw bind rejected")
