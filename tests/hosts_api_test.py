#!/usr/bin/env python3
"""Hosts auth/confirmation contract. Never modifies the host's Hosts file."""
import http.client
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time

TOKEN = "hosts-fixture-token"
binary = str(Path(sys.argv[1]).resolve())

def port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]

def request(p, path, body=None, auth=None):
    c = http.client.HTTPConnection("127.0.0.1", p, timeout=6)
    headers = {"Content-Type": "application/json"}
    if auth:
        headers["Authorization"] = "Bearer " + auth
    try:
        c.request("GET" if body is None else "POST", path,
                  None if body is None else json.dumps(body), headers)
        r = c.getresponse()
        raw = r.read()
        try:
            value = json.loads(raw) if raw else {}
        except ValueError:
            value = raw.decode()
        return r.status, value
    finally:
        c.close()

with tempfile.TemporaryDirectory(prefix="hosts-api-") as tmp:
    folder = Path(tmp)
    token = folder / "token"
    token.write_text(TOKEN)
    token.chmod(0o600)
    local, lan = port(), port()
    before = Path("/etc/hosts").read_bytes()
    env = dict(os.environ, ZWRT_DATAD_UBUS_BIN="/usr/bin/false",
               ZWRT_DATAD_UCI_BIN="/usr/bin/false")
    proc = subprocess.Popen([binary, "--data-dir", tmp, "--port", str(local),
                             "--lan-bind", "127.0.0.1", "--lan-port", str(lan),
                             "--auth-token-file", str(token)], env=env,
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        for _ in range(100):
            try:
                if request(local, "/healthz")[0] == 200:
                    break
            except OSError:
                pass
            assert proc.poll() is None
            time.sleep(.05)
        else:
            raise AssertionError("daemon startup failed")
        controls = request(local, "/capabilities")[1]["controls"]
        assert {"hosts.status", "hosts.save", "hosts.restore"} <= set(controls)
        assert not {"time.status", "time.config.set", "time.sync", "files.status"} & set(controls)
        for p in (local, lan):
            for action in ("hosts.status", "hosts.save", "hosts.restore"):
                body = {"action": action, "params": {}}
                for auth in (None, "wrong"):
                    assert request(p, "/control", body, auth)[0] == 401
                assert request(p, "/control?access_token=" + TOKEN, body)[0] == 401
            code, status = request(p, "/control", {"action": "hosts.status", "params": {}}, TOKEN)
            assert code == 200 and status["ok"] and status["result"]["supported"]
            assert request(p, "/control", {"action":"hosts.status","params":{"path":"/tmp/x"}}, TOKEN)[0] == 400
            for action in ("hosts.save", "hosts.restore"):
                body = {"action":action,"params":{"expected_revision":"0"*64}}
                if action == "hosts.save":
                    body["params"]["content"] = "# Must never be written by this test\n"
                assert request(p, "/control", body, TOKEN)[0] == 400
                # Invalid revision is rejected before opening a mount or backup.
                body["confirmed"] = True
                body["params"]["expected_revision"] = "bad"
                code, failure = request(p, "/control", body, TOKEN)
                assert code == 409 and failure["error"]["code"] == "invalid_parameter"
            assert "hosts_config" not in request(p, "/state", auth=TOKEN)[1]
        assert not (folder / "hosts-control").exists()
        assert Path("/etc/hosts").read_bytes() == before
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=8)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()
print("Hosts: both listeners require header auth, writes require confirmation, invalid writes have no effects")
