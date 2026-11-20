#!/usr/bin/env python3
"""Exercise the real streamer against a threaded fake libdiag, never a modem."""
import json
import os
from pathlib import Path
import signal
import socket
import struct
import subprocess
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]
MOCK = r'''
#include <pthread.h>
#include <stdatomic.h>
#include <stdlib.h>
#include <unistd.h>
static void (*callback)(unsigned char *, int);
static pthread_t thread;
static atomic_int enabled, stopped;
static int created, attempts;
static void *pump(void *unused) {
    (void)unused;
    unsigned char packet[12] = {12,0,0x21,0xb8,1,2,3,4,5,6,7,8};
    while (!atomic_load(&stopped)) {
        if (atomic_load(&enabled) && access(getenv("MOCK_FLOW"), F_OK) == 0)
            callback(packet, sizeof(packet));
        usleep(10000);
    }
    return 0;
}
int Diag_LSM_Init(unsigned char *name) { (void)name; return 1; }
int diag_lsm_dci_init(void) { return 1001; }
int diag_register_dci_client(int *id, unsigned short *p, int a, void *b) {
    (void)p; (void)a; (void)b; *id=7; return 1001;
}
int diag_register_dci_stream_proc(int id, void (*cb)(unsigned char *,int), void (*ev)(unsigned char *,int)) {
    (void)id; (void)ev;
    if (getenv("MOCK_STREAM_FAIL")) return -1;
    callback=cb; created=1; pthread_create(&thread,0,pump,0); return 1001;
}
int diag_log_stream_config(int id, int on, unsigned short *codes, int n) {
    (void)id; (void)codes; (void)n;
    const char *fail=getenv("MOCK_CONFIG_FAILURES");
    if (on && fail && attempts++ < atoi(fail)) return -1;
    atomic_store(&enabled,on); return 1001;
}
int diag_release_dci_client(int *id) {
    (void)id; atomic_store(&stopped,1); if (created) pthread_join(thread,0); return 1001;
}
int Diag_LSM_DeInit(void) { return 1; }
'''


class StreamerTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.build = tempfile.TemporaryDirectory(prefix="signal-build-")
        cls.root = Path(cls.build.name)
        cls.worker = cls.root / "streamer"
        (cls.root / "mock.c").write_text(MOCK)
        subprocess.run(["cc", "-O2", "-Wall", "-Wextra", "-Werror", "-pthread",
                        str(ROOT / "rust/u50_diag_streamer.c"), "-ldl", "-o", str(cls.worker)], check=True)
        subprocess.run(["cc", "-shared", "-fPIC", "-pthread", str(cls.root / "mock.c"),
                        "-o", str(cls.root / "libdiag.so.1")], check=True)

    @classmethod
    def tearDownClass(cls):
        cls.build.cleanup()

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="signal-case-")
        self.path = Path(self.temp.name)
        self.status = self.path / "status"
        self.heartbeat = self.path / "heartbeat"
        self.heartbeat.touch()
        self.flow = self.path / "flow"
        self.process = None
        self.sockets = []

    def tearDown(self):
        for s in self.sockets:
            s.close()
        if self.process and self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=4)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()
        self.temp.cleanup()

    def start(self, **extras):
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            self.port = sock.getsockname()[1]
        env = dict(os.environ, LD_LIBRARY_PATH=str(self.root), MOCK_FLOW=str(self.flow), **extras)
        self.process = subprocess.Popen([str(self.worker), str(self.status), str(self.heartbeat),
                                         str(self.port)], env=env)

    def wait_status(self, predicate, timeout=5):
        end = time.monotonic() + timeout
        latest = None
        while time.monotonic() < end:
            self.heartbeat.touch()
            try:
                latest = json.loads(self.status.read_text())
                if predicate(latest):
                    return latest
            except (OSError, ValueError):
                pass
            time.sleep(.03)
        self.fail(f"status timeout: {latest}, exit={self.process.poll()}")

    def connect(self):
        sock = socket.create_connection(("127.0.0.1", self.port), timeout=4)
        self.sockets.append(sock)
        return sock

    def frame(self, sock):
        raw = b""
        while len(raw) < 20:
            part = sock.recv(20 - len(raw))
            self.assertTrue(part, "relay closed unexpectedly")
            raw += part
        self.assertEqual(struct.unpack("<II", raw[:8]), (12, 0xB821))
        self.assertEqual(raw[8:], bytes([12, 0, 0x21, 0xB8, 1, 2, 3, 4, 5, 6, 7, 8]))

    def test_idle_disconnects_release_slots_without_packets(self):
        self.start()
        self.wait_status(lambda s: s["state"] == "running")
        for _ in range(8):
            sock = self.connect()
            self.wait_status(lambda s: s["clients"] == 1)
            sock.close()
            self.wait_status(lambda s: s["clients"] == 0 and s["subscribed"] == 0)
        sock = self.connect()
        self.flow.touch()
        self.frame(sock)

    def test_disconnect_churn_does_not_break_another_consumer(self):
        self.start()
        self.wait_status(lambda s: s["state"] == "running")
        primary = self.connect()
        self.flow.touch()
        for _ in range(40):
            sock = self.connect()
            sock.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0))
            sock.close()
            self.frame(primary)
        self.assertIsNone(self.process.poll())

    def test_subscription_retries_then_delivers_packets(self):
        self.start(MOCK_CONFIG_FAILURES="2")
        self.wait_status(lambda s: s["state"] == "running")
        sock = self.connect()
        self.flow.touch()
        self.wait_status(lambda s: s["state"] == "retrying" and s["subscribed"] == 0)
        self.wait_status(lambda s: s["state"] == "running" and s["subscribed"] == 1)
        self.frame(sock)

    def test_permanent_subscription_failure_is_reported(self):
        self.start(MOCK_CONFIG_FAILURES="100")
        self.wait_status(lambda s: s["state"] == "running")
        self.connect()
        self.wait_status(lambda s: s["state"] == "error" and s["reason"] == "log_config_failed", 7)
        self.assertNotEqual(self.process.wait(timeout=2), 0)

    def test_callback_registration_failure_is_not_ready(self):
        self.start(MOCK_STREAM_FAIL="1")
        self.wait_status(lambda s: s["state"] == "error" and s["reason"] == "dci_stream_failed")
        self.assertEqual(self.process.wait(timeout=2), 71)

    def test_callback_to_closed_socket_does_not_raise_sigpipe(self):
        # Directly exercise EPIPE with default signal disposition. No timing or
        # libc-specific TCP reset behavior is needed to reproduce the old crash.
        source = self.path / "pipe.c"
        source.write_text('#define main streamer_main\n#include "' +
                          str(ROOT / 'rust/u50_diag_streamer.c') + '"\n#undef main\n' + r'''
int main(void) {
    int fd[2]; if (socketpair(AF_UNIX,SOCK_STREAM,0,fd)) return 1;
    signal(SIGPIPE,SIG_DFL);
    cli[0]=fd[0]; close(fd[1]);
    unsigned char p[12]={12,0,0x21,0xb8}; logs_cb(p,12);
    return cli[0] == -1 ? 0 : 2;
}
''')
        binary = self.path / "pipe"
        subprocess.run(["cc", "-O2", "-pthread", str(source), "-ldl", "-o", str(binary)], check=True)
        self.assertEqual(subprocess.run([str(binary)], timeout=3).returncode, 0)


if __name__ == "__main__":
    unittest.main(verbosity=2)
