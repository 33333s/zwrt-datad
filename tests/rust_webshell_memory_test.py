#!/usr/bin/env python3
"""Bounded-memory and resource-reclamation smoke test for Rust WebShell."""

import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time

from rust_webshell_test import (
    BIN,
    TOKEN,
    free_port,
    http_status,
    masked_frame,
    recv_frame,
    recv_until,
    websocket,
)


def rss_kib(pid):
    status = Path(f"/proc/{pid}/status")
    if status.exists():
        for line in status.read_text().splitlines():
            if line.startswith("VmRSS:"):
                return int(line.split()[1])
    return int(subprocess.check_output(["ps", "-o", "rss=", "-p", str(pid)], text=True))


def fd_count(pid):
    directory = Path(f"/proc/{pid}/fd")
    return len(list(directory.iterdir())) if directory.exists() else None


def wait_zero(port, timeout=3):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        with http_status(port, "/webshell/status", TOKEN) as response:
            if json.load(response)["active_sessions"] == 0:
                return
        time.sleep(0.01)
    raise AssertionError("WebShell session did not release")


def one_session(port):
    sock, header, pending, _ = websocket(port)
    assert header.startswith(b"HTTP/1.1 101"), header
    recv_frame(sock, pending)
    sock.sendall(masked_frame(2, b"printf '__MEM_OK__\\n'\n"))
    recv_until(sock, b"__MEM_OK__")
    sock.close()
    wait_zero(port)


def main():
    port = free_port()
    with tempfile.TemporaryDirectory(prefix="datad-webshell-memory-") as tmp:
        root = Path(tmp)
        token_file = root / "auth.token"
        token_file.write_text(TOKEN)
        fake_ubus = root / "ubus"
        fake_ubus.write_text("#!/bin/sh\nprintf '{}\\n'\n")
        fake_ubus.chmod(0o755)
        env = dict(
            os.environ,
            ZWRT_DATAD_UBUS_BIN=str(fake_ubus),
            ZWRT_DATAD_UCI_BIN="/usr/bin/false",
            ZWRT_DATAD_DIR=str(root / "data"),
            ZWRT_DATAD_OTA_DISABLE_AUTO="1",
        )
        proc = subprocess.Popen(
            [str(BIN), "-i", "500", "-p", str(port),
             "--auth-token-file", str(token_file), "--webshell"],
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            text=True,
        )
        try:
            deadline = time.monotonic() + 8
            while time.monotonic() < deadline:
                try:
                    with http_status(port, "/healthz") as response:
                        if response.status == 200:
                            break
                except OSError:
                    time.sleep(0.05)
            else:
                raise AssertionError("datad did not start")

            for _ in range(8):
                one_session(port)
            baseline_rss = rss_kib(proc.pid)
            baseline_fds = fd_count(proc.pid)

            for _ in range(128):
                one_session(port)
            after_rss = rss_kib(proc.pid)
            after_fds = fd_count(proc.pid)
            assert after_rss - baseline_rss <= 12 * 1024, (baseline_rss, after_rss)
            if baseline_fds is not None:
                assert after_fds <= baseline_fds + 2, (baseline_fds, after_fds)

            slow, header, pending, _ = websocket(port)
            assert header.startswith(b"HTTP/1.1 101"), header
            recv_frame(slow, pending)
            slow.sendall(masked_frame(2, b"yes x\n"))
            time.sleep(1)
            pressured_rss = rss_kib(proc.pid)
            assert pressured_rss - baseline_rss <= 12 * 1024, (baseline_rss, pressured_rss)
            slow.shutdown(socket.SHUT_RDWR)
            slow.close()
            wait_zero(port)

            print(json.dumps({
                "sessions": 128,
                "rss_baseline_kib": baseline_rss,
                "rss_after_kib": after_rss,
                "rss_pressured_kib": pressured_rss,
                "fd_baseline": baseline_fds,
                "fd_after": after_fds,
            }, sort_keys=True))
        finally:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()


if __name__ == "__main__":
    main()
