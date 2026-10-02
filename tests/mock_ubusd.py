#!/usr/bin/env python3
"""Fake ubusd for the integration suites (Linux only).

Speaks the ubus socket protocol (HELLO, LOOKUP, INVOKE, DATA, STATUS) and
answers every INVOKE by running the ubus CLI mock that the connecting datad
process was configured with (`ZWRT_DATAD_UBUS_BIN` read from its environment
through SO_PEERCRED), so every existing fixture works unchanged over the
socket path. The CLI exit code maps back to the ubus status the real CLI
would have exited with.

Usage: mock_ubusd.py SOCKET_PATH COUNT_FILE
"""
import json
import os
import socket
import socketserver
import struct
import subprocess
import sys
import threading

HELLO, STATUS, DATA, LOOKUP, INVOKE = 0, 1, 2, 4, 5
A_STATUS, A_OBJPATH, A_OBJID, A_METHOD, A_DATA = 1, 2, 3, 4, 7
UNSPEC, ARRAY, TABLE, STRING, INT64, INT32, INT16, INT8, DOUBLE = range(9)
NOT_FOUND = 4

socket_path, count_file = sys.argv[1:3]
lock = threading.Lock()
ids = {}
invokes = 0


def align(n):
    return (n + 3) & ~3


def attr(ident, payload, extended=False):
    length = 4 + len(payload)
    head = (ident << 24) | length | (0x80000000 if extended else 0)
    return struct.pack('>I', head) + payload + b'\0' * (align(length) - length)


def attrs(buf):
    out = []
    while buf:
        head, = struct.unpack('>I', buf[:4])
        length = head & 0xffffff
        out.append(((head >> 24) & 0x7f, bool(head & 0x80000000), buf[4:length]))
        buf = buf[align(length):]
    return out


def field(kind, name, data):
    raw = name.encode()
    header = struct.pack('>H', len(raw)) + raw + b'\0'
    header += b'\0' * (align(len(header)) - len(header))
    return attr(kind, header + data, True)


def encode(name, value):
    if value is None:
        return field(UNSPEC, name, b'')
    if isinstance(value, bool):
        return field(INT8, name, bytes([int(value)]))
    if isinstance(value, int):
        if -2**31 <= value < 2**31:
            return field(INT32, name, struct.pack('>i', value))
        return field(INT64, name, struct.pack('>q', value))
    if isinstance(value, float):
        return field(DOUBLE, name, struct.pack('>d', value))
    if isinstance(value, str):
        return field(STRING, name, value.encode() + b'\0')
    if isinstance(value, list):
        return field(ARRAY, name, b''.join(encode('', item) for item in value))
    return field(TABLE, name, table(value))


def table(obj):
    return b''.join(encode(key, value) for key, value in obj.items())


def decode_field(payload, kind):
    namelen, = struct.unpack('>H', payload[:2])
    name = payload[2:2 + namelen].decode()
    data = payload[align(2 + namelen + 1):]
    if kind == UNSPEC:
        return name, None
    if kind == ARRAY:
        return name, [decode_field(p, k)[1] for k, _, p in attrs(data)]
    if kind == TABLE:
        return name, decode_table(data)
    if kind == STRING:
        return name, data.split(b'\0', 1)[0].decode()
    if kind == INT8:
        return name, data[0] != 0
    fmt = {INT16: '>h', INT32: '>i', INT64: '>q', DOUBLE: '>d'}[kind]
    return name, struct.unpack(fmt, data)[0]


def decode_table(buf):
    return dict(decode_field(payload, kind) for kind, _, payload in attrs(buf))


def frame(kind, seq, peer, body):
    return struct.pack('>BBHI', 0, kind, seq, peer) + attr(0, body)


def peer_environment(conn):
    creds = conn.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, struct.calcsize('3i'))
    pid, _, _ = struct.unpack('3i', creds)
    with open(f'/proc/{pid}/environ', 'rb') as handle:
        pairs = handle.read().split(b'\0')
    return dict(p.decode(errors='replace').split('=', 1) for p in pairs if b'=' in p)


class Handler(socketserver.BaseRequestHandler):
    def read_exact(self, size):
        data = b''
        while len(data) < size:
            chunk = self.request.recv(size - len(data))
            if not chunk:
                raise EOFError
            data += chunk
        return data

    def send(self, kind, seq, peer, body=b''):
        self.request.sendall(frame(kind, seq, peer, body))

    def status(self, seq, peer, code):
        self.send(STATUS, seq, peer, attr(A_STATUS, struct.pack('>I', code)))

    def handle(self):
        global invokes
        env = peer_environment(self.request)
        cli = env.get('ZWRT_DATAD_UBUS_BIN', '/bin/ubus')
        self.send(HELLO, 0, 0x1000)
        while True:
            try:
                head = self.read_exact(12)
            except EOFError:
                return
            _, kind, seq, peer, blob = struct.unpack('>BBHII', head)
            body = self.read_exact((blob & 0xffffff) - 4)
            parts = {ident: payload for ident, _, payload in attrs(body)}
            if kind == LOOKUP:
                name = parts[A_OBJPATH].split(b'\0', 1)[0].decode()
                with lock:
                    ident = ids.setdefault(name, 0x2000 + len(ids))
                reply = attr(A_OBJPATH, name.encode() + b'\0') + attr(A_OBJID, struct.pack('>I', ident))
                self.send(DATA, seq, 0, reply)
                self.status(seq, 0, 0)
            elif kind == INVOKE:
                ident, = struct.unpack('>I', parts[A_OBJID])
                with lock:
                    name = next((n for n, i in ids.items() if i == ident), None)
                    invokes += 1
                    with open(count_file, 'w') as handle:
                        handle.write(str(invokes))
                if name is None:
                    self.status(seq, peer, NOT_FOUND)
                    continue
                method = parts[A_METHOD].split(b'\0', 1)[0].decode()
                args = decode_table(parts.get(A_DATA, b''))
                done = subprocess.run([cli, 'call', name, method, json.dumps(args)], env=env,
                                      stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, timeout=30)
                if done.returncode != 0:
                    self.status(seq, peer, (256 - done.returncode) % 256)
                    continue
                if done.stdout.strip():
                    self.send(DATA, seq, peer, attr(A_DATA, table(json.loads(done.stdout))))
                self.status(seq, peer, 0)
            else:
                self.status(seq, peer, 1)


class Server(socketserver.ThreadingMixIn, socketserver.UnixStreamServer):
    daemon_threads = True


if os.path.exists(socket_path):
    os.unlink(socket_path)
with open(count_file, 'w') as handle:
    handle.write('0')
Server(socket_path, Handler).serve_forever()
