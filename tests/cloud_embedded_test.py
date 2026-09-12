#!/usr/bin/env python3
import json
import os
import signal
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def request(url, method="GET", body=None):
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(url, data=data, method=method)
    if data is not None:
        req.add_header("Content-Type", "application/json")
    with urllib.request.urlopen(req, timeout=3) as response:
        return response.status, json.load(response)


def wait_ready(url):
    for _ in range(100):
        try:
            with urllib.request.urlopen(url + "/healthz", timeout=3) as response:
                if response.status == 200 and response.read() == b"ok\n":
                    return
        except (OSError, urllib.error.URLError):
            time.sleep(0.05)
    raise RuntimeError("datad did not become ready")


def main():
    binary = Path(sys.argv[1]).resolve()
    with tempfile.TemporaryDirectory() as folder:
        port = free_port()
        base = f"http://127.0.0.1:{port}"
        env = os.environ.copy()
        env["ZWRT_DATAD_DIR"] = folder
        proc = subprocess.Popen(
            [str(binary), "-b", "127.0.0.1", "-p", str(port), "-i", "500"],
            env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True,
        )
        try:
            wait_ready(base)
            code, status = request(base + "/cloud/status")
            assert code == 200 and status["state"] == "disabled", status
            code, config = request(base + "/cloud/config")
            assert code == 200 and config["password_configured"] is False, config
            saved = config["config"]
            saved["password"] = "fixture-secret"
            code, configured = request(base + "/cloud/config", "POST", saved)
            assert code == 200 and configured["password_configured"] is True, configured
            config_file = Path(folder) / "cloud.json"
            assert config_file.exists() and config_file.stat().st_mode & 0o777 == 0o600
            assert not (Path(folder) / "cloud.sock").exists()
            time.sleep(0.2)
            assert proc.poll() is None
        finally:
            proc.send_signal(signal.SIGTERM)
            try:
                proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()
        assert proc.returncode == 0, proc.stderr.read()

        config_file.write_text("{invalid", encoding="utf-8")
        config_file.chmod(0o600)
        proc = subprocess.Popen(
            [str(binary), "-b", "127.0.0.1", "-p", str(port), "-i", "500"],
            env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True,
        )
        try:
            wait_ready(base)
            _, status = request(base + "/cloud/status")
            assert status["state"] == "error", status
            code, _ = request(base + "/cloud/config", "POST", saved)
            assert code == 200
            for _ in range(50):
                _, status = request(base + "/cloud/status")
                if status["state"] == "disabled":
                    break
                time.sleep(0.02)
            assert status["state"] == "disabled", status
        finally:
            proc.send_signal(signal.SIGTERM)
            try:
                proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()
        assert proc.returncode == 0, proc.stderr.read()


if __name__ == "__main__":
    main()
