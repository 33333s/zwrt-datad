#!/usr/bin/env python3
"""Local HTTP time contract; never changes the host clock, OEM service or timezone."""
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

binary = sys.argv[1] if len(sys.argv) > 1 else "rust/target/debug/zwrt-datad"

def call(port, action, params, confirmed=True):
    data = json.dumps({"action": action, "params": params, "confirmed": confirmed}).encode()
    req = urllib.request.Request(f"http://127.0.0.1:{port}/control", data=data,
                                 headers={"Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=5) as res:
            return res.status, json.load(res)
    except urllib.error.HTTPError as res:
        return res.code, json.load(res)

with tempfile.TemporaryDirectory(prefix="datad-time-http-") as temp:
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        port = probe.getsockname()[1]
    proc = subprocess.Popen([binary, "--bind", "127.0.0.1", "--port", str(port), "--data-dir", temp],
                            env={**os.environ, "ZWRT_DATAD_OTA_DISABLE_AUTO": "1"},
                            stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    try:
        for _ in range(100):
            try:
                status, before = call(port, "time.status", {})
                if status == 200:
                    break
            except urllib.error.URLError:
                time.sleep(0.05)
        else:
            raise AssertionError("HTTP daemon did not become ready")
        before = before["result"]
        assert before["calibration_enabled"] is False and before["boot_sync_enabled"] is True
        assert before["last_operation"] is None and before["recent_operations"] == []
        assert before["authenticated"] is False and isinstance(before["write_supported"], bool)
        assert call(port, "time.status", {}, confirmed=False)[0] == 200
        assert call(port, "time.status", {"extra": 1})[0] == 400
        assert call(port, "time.config.set", {"server": "ntp.example.test"}, confirmed=False)[0] == 400
        assert call(port, "time.sync", {}, confirmed=False)[0] == 400
        for params in ({"server": "https://x"}, {"server": "u@x"}, {"server": "x:0"},
                       {"server": "x", "extra": True}, {"server": "x", "operation_tag": "bad"},
                       {"calibration_enabled": "yes"}, {"server": "x", "boot_sync_enabled": None}):
            assert call(port, "time.config.set", params)[0] == 400, params
        tag = "ABCDEF0123456789" * 2
        params = {"operation_tag": tag, "server": "ntp.example.test:124", "boot_sync_enabled": False}
        code, ack = call(port, "time.config.set", params)
        assert code == 200 and ack["result"]["operation_tag"] == tag
        assert ack["result"]["configuration_saved"] is True and ack["result"]["verified"] is None
        for _ in range(100):
            _, current = call(port, "time.status", {})
            current = current["result"]
            if current["last_operation"]["phase"] == "succeeded":
                break
            time.sleep(0.02)
        else:
            raise AssertionError(current)
        assert current["last_operation"]["verified"] is True
        assert current["last_sync"] is None and current["guard_active"] is False
        assert current["clock_generation"] == before["clock_generation"], "configuration must not change the clock"
        config = Path(temp) / "time-control" / "config.json"
        assert config.stat().st_mode & 0o777 == 0o600
        assert json.loads(config.read_text())["server"] == "ntp.example.test:124"
        assert not (config.parent / "time-ownership.json").exists()
        assert call(port, "time.config.set", params)[0] == 200
        assert call(port, "time.config.set", {**params, "server": "different.test"})[0] == 400
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
print("Time HTTP: closed parameters, confirmation, fast saved ACK, verified receipt and no hardware changes OK")
