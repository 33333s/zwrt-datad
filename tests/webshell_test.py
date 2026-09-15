#!/usr/bin/env python3
"""Protocol and lifecycle regression tests for the loopback-only WebShell."""

import base64
import hashlib
import json
import os
from pathlib import Path
import re
import socket
import struct
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request


BIN = Path(sys.argv[1]).resolve()
TOKEN = "fixture-webshell-token"
GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def http_status(port, path, token=None):
    headers = {"Authorization": f"Bearer {token}"} if token else {}
    request = urllib.request.Request(f"http://127.0.0.1:{port}{path}", headers=headers)
    try:
        return urllib.request.urlopen(request, timeout=3)
    except urllib.error.HTTPError as error:
        return error


def websocket(port, token=TOKEN, key=None, path="/webshell"):
    key = key or base64.b64encode(os.urandom(16)).decode()
    sock = socket.create_connection(("127.0.0.1", port), timeout=3)
    lines = [
        f"GET {path} HTTP/1.1",
        f"Host: 127.0.0.1:{port}",
        "Upgrade: websocket",
        "Connection: keep-alive, Upgrade",
        "Sec-WebSocket-Version: 13",
        f"Sec-WebSocket-Key: {key}",
    ]
    if token is not None:
        lines.append(f"Authorization: Bearer {token}")
    sock.sendall(("\r\n".join(lines) + "\r\n\r\n").encode())
    response = b""
    while b"\r\n\r\n" not in response:
        chunk = sock.recv(4096)
        if not chunk:
            break
        response += chunk
    header, _, surplus = response.partition(b"\r\n\r\n")
    return sock, header, surplus, key


def masked_frame(opcode, payload=b"", fin=True):
    if isinstance(payload, str):
        payload = payload.encode()
    first = (0x80 if fin else 0) | opcode
    mask = os.urandom(4)
    length = len(payload)
    if length <= 125:
        header = bytes((first, 0x80 | length))
    elif length <= 65535:
        header = bytes((first, 0x80 | 126)) + struct.pack("!H", length)
    else:
        header = bytes((first, 0x80 | 127)) + struct.pack("!Q", length)
    encoded = bytes(value ^ mask[index & 3] for index, value in enumerate(payload))
    return header + mask + encoded


def recv_frame(sock, initial=b""):
    data = bytearray(initial)
    while len(data) < 2:
        data.extend(sock.recv(4096))
    opcode = data[0] & 0x0F
    assert data[0] & 0x80, data[:2]
    length = data[1] & 0x7F
    offset = 2
    assert not data[1] & 0x80, data[:2]
    if length == 126:
        while len(data) < 4:
            data.extend(sock.recv(4096))
        length, offset = struct.unpack("!H", data[2:4])[0], 4
    elif length == 127:
        while len(data) < 10:
            data.extend(sock.recv(4096))
        length, offset = struct.unpack("!Q", data[2:10])[0], 10
    while len(data) < offset + length:
        data.extend(sock.recv(4096))
    return opcode, bytes(data[offset:offset + length]), bytes(data[offset + length:])


def recv_until(sock, marker, initial=b"", timeout=5):
    pending = initial
    output = bytearray()
    deadline = time.monotonic() + timeout
    while marker not in output and time.monotonic() < deadline:
        sock.settimeout(max(0.1, deadline - time.monotonic()))
        opcode, payload, pending = recv_frame(sock, pending)
        if opcode == 2:
            output.extend(payload)
    assert marker in output, bytes(output)
    return bytes(output), pending


def recv_until_count(sock, marker, count, initial=b"", timeout=5):
    pending = initial
    output = bytearray()
    deadline = time.monotonic() + timeout
    while output.count(marker) < count and time.monotonic() < deadline:
        sock.settimeout(max(0.1, deadline - time.monotonic()))
        opcode, payload, pending = recv_frame(sock, pending)
        if opcode == 2:
            output.extend(payload)
    assert output.count(marker) >= count, bytes(output)
    return bytes(output), pending


def recv_until_pattern(sock, pattern, initial=b"", timeout=5):
    pending = initial
    output = bytearray()
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        match = re.search(pattern, output)
        if match:
            return bytes(output), pending, match
        sock.settimeout(max(0.1, deadline - time.monotonic()))
        opcode, payload, pending = recv_frame(sock, pending)
        if opcode == 2:
            output.extend(payload)
    raise AssertionError(bytes(output))


def wait_closed(sock, timeout=3):
    sock.settimeout(0.2)
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            if not sock.recv(4096):
                return
        except (ConnectionResetError, BrokenPipeError):
            return
        except socket.timeout:
            pass
    raise AssertionError("WebSocket stayed open after a protocol violation")


def main():
    local_port, lan_port = free_port(), free_port()
    with tempfile.TemporaryDirectory(prefix="datad-webshell-") as tmp:
        root = Path(tmp)
        token_file = root / "auth.token"
        token_file.write_text(TOKEN)
        fake_ubus = root / "ubus"
        fake_ubus.write_text("#!/bin/sh\nprintf '{}\\n'\n")
        fake_ubus.chmod(0o755)
        env = dict(os.environ, ZWRT_DATAD_UBUS_BIN=str(fake_ubus),
                   ZWRT_DATAD_UCI_BIN="/usr/bin/false", ZWRT_DATAD_DIR=str(root / "data"),
                   ZWRT_DATAD_NEIGHBOR_DIR=str(root / "neighbor"),
                   WEBSHELL_SECRET_SENTINEL="must-not-reach-shell")
        proc = subprocess.Popen([
            str(BIN), "-i", "100", "-p", str(local_port),
            "--lan-bind", "127.0.0.1", "--lan-port", str(lan_port),
            "--auth-token-file", str(token_file), "--webshell",
        ], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
        try:
            deadline = time.monotonic() + 8
            while True:
                try:
                    with http_status(local_port, "/healthz") as response:
                        assert response.status == 200
                    break
                except OSError:
                    if proc.poll() is not None or time.monotonic() >= deadline:
                        raise AssertionError("datad did not start")
                    time.sleep(0.05)

            with http_status(local_port, "/webshell/status") as response:
                assert response.status == 401
            with http_status(local_port, "/webshell/status?access_token=" + TOKEN) as response:
                assert response.status == 401
            with http_status(local_port, "/webshell/status", TOKEN) as response:
                assert json.load(response) == {
                    "enabled": True, "active_sessions": 0, "max_sessions": 4,
                    "protocol": "websocket-binary-v1",
                }
            with http_status(lan_port, "/webshell/status", TOKEN) as response:
                assert response.status == 403

            unauth, header, _, _ = websocket(local_port, token=None)
            assert header.startswith(b"HTTP/1.1 401"), header
            unauth.close()
            lan, header, _, _ = websocket(lan_port)
            assert header.startswith(b"HTTP/1.1 403"), header
            lan.close()
            invalid, header, _, _ = websocket(local_port, key="invalid")
            assert header.startswith(b"HTTP/1.1 400"), header
            invalid.close()
            noncanonical, header, _, _ = websocket(local_port, key="AAAAAAAAAAAAAAAAAAAAAB==")
            assert header.startswith(b"HTTP/1.1 400"), header
            noncanonical.close()

            shell, header, pending, key = websocket(local_port)
            assert header.startswith(b"HTTP/1.1 101"), header
            accept = base64.b64encode(hashlib.sha1((key + GUID).encode()).digest())
            assert b"Sec-WebSocket-Accept: " + accept in header, header
            opcode, ready, pending = recv_frame(shell, pending)
            assert opcode == 1 and json.loads(ready) == {"type": "ready", "cols": 80, "rows": 24}
            shell.sendall(masked_frame(2, b"env; printf '__ENV_END__\\n'\n"))
            env_output, pending = recv_until(shell, b"__ENV_END__", pending)
            assert b"must-not-reach-shell" not in env_output, env_output
            shell.sendall(masked_frame(1, '{"type":"resize","cols":93,"rows":31}'))
            shell.sendall(masked_frame(2, b"stty size; printf '__PID__%s__\\n' \"$$\"\n"))
            output, pending, pid_match = recv_until_pattern(shell, rb"__PID__([0-9]+)__", pending)
            assert b"31 93" in output, output
            child_pid = int(pid_match.group(1))
            if sys.platform.startswith("linux"):
                shell.sendall(masked_frame(
                    2,
                    b"for f in /proc/$$/fd/*; do readlink \"$f\"; done; printf '__FD_END__\\n'\n",
                ))
                fd_output, pending = recv_until(shell, b"__FD_END__", pending)
                assert b"socket:[" not in fd_output, fd_output
            shell.close()
            deadline = time.monotonic() + 3
            while time.monotonic() < deadline:
                try:
                    os.kill(child_pid, 0)
                except ProcessLookupError:
                    break
                time.sleep(0.05)
            else:
                raise AssertionError(f"shell child {child_pid} survived disconnect")

            bad, header, pending, _ = websocket(local_port)
            assert header.startswith(b"HTTP/1.1 101"), header
            recv_frame(bad, pending)
            bad.sendall(b"\x82\x01x")
            wait_closed(bad)
            bad.close()

            huge, header, pending, _ = websocket(local_port)
            assert header.startswith(b"HTTP/1.1 101"), header
            recv_frame(huge, pending)
            huge.sendall(b"\x82\xff" + struct.pack("!Q", 65536) + b"abcd")
            wait_closed(huge)
            huge.close()

            fragmented, header, pending, _ = websocket(local_port)
            assert header.startswith(b"HTTP/1.1 101"), header
            recv_frame(fragmented, pending)
            fragmented.sendall(masked_frame(2, b"x", fin=False))
            wait_closed(fragmented)
            fragmented.close()

            invalid_resize, header, pending, _ = websocket(local_port)
            assert header.startswith(b"HTTP/1.1 101"), header
            recv_frame(invalid_resize, pending)
            invalid_resize.sendall(masked_frame(1, '{"type":"resize","cols":9999,"rows":1}'))
            wait_closed(invalid_resize)
            invalid_resize.close()

            trailing_resize, header, pending, _ = websocket(local_port)
            assert header.startswith(b"HTTP/1.1 101"), header
            recv_frame(trailing_resize, pending)
            trailing_resize.sendall(masked_frame(
                1, '{"type":"resize","cols":80,"rows":24}garbage'
            ))
            wait_closed(trailing_resize)
            trailing_resize.close()

            ping, header, pending, _ = websocket(local_port)
            assert header.startswith(b"HTTP/1.1 101"), header
            recv_frame(ping, pending)
            ping.sendall(masked_frame(9, b"probe"))
            deadline = time.monotonic() + 3
            while True:
                ping.settimeout(max(0.1, deadline - time.monotonic()))
                opcode, payload, _ = recv_frame(ping)
                if opcode == 10:
                    assert payload == b"probe", payload
                    break
                assert opcode == 2, (opcode, payload)
                if time.monotonic() >= deadline:
                    raise AssertionError("WebSocket pong was not received")
            ping.close()

            sessions = []
            for _ in range(4):
                sock, header, pending, _ = websocket(local_port)
                assert header.startswith(b"HTTP/1.1 101"), header
                recv_frame(sock, pending)
                sessions.append(sock)
            extra, header, _, _ = websocket(local_port)
            assert header.startswith(b"HTTP/1.1 503"), header
            extra.close()
            with http_status(local_port, "/webshell/status", TOKEN) as response:
                assert json.load(response)["active_sessions"] == 4
            for sock in sessions:
                sock.close()

            for _ in range(32):
                sock, header, pending, _ = websocket(local_port)
                assert header.startswith(b"HTTP/1.1 101"), header
                recv_frame(sock, pending)
                sock.sendall(masked_frame(2, b"printf '__LOOP_OK__\\n'\n"))
                recv_until(sock, b"__LOOP_OK__")
                sock.close()
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline:
                with http_status(local_port, "/webshell/status", TOKEN) as response:
                    if json.load(response)["active_sessions"] == 0:
                        break
                time.sleep(0.05)
            else:
                raise AssertionError("WebShell sessions were not reclaimed")

            print("webshell protocol, auth, PTY, resize, limits, and cleanup tests passed")
        finally:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()
            stderr = proc.stderr.read()
            assert "Address already in use" not in stderr, stderr


if __name__ == "__main__":
    main()
