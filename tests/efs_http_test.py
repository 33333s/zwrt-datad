#!/usr/bin/env python3
"""EFS HTTP/auth integration. Uses a fake libdiag and never accesses a modem."""
import concurrent.futures
import http.client
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time
from urllib.parse import urlencode

ROOT = Path(__file__).resolve().parents[1]
BIN = str(Path(sys.argv[1]).resolve())
TOKEN = "efs-api-fixture-token"
AUTH = {"Authorization": "Bearer " + TOKEN}
CONTENT = bytes(range(256)) * 2


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def request(port, path, method="GET", body=None, headers=None):
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
    try:
        try:
            conn.request(method, path, body, headers or {})
        except BrokenPipeError:
            # A size rejection may close the write side before the large body
            # finishes sending. Still require the actual HTTP status below.
            pass
        response = conn.getresponse()
        return response.status, dict(response.getheaders()), response.read()
    finally:
        conn.close()


with tempfile.TemporaryDirectory(prefix="datad-efs-") as temporary:
    directory = Path(temporary)
    worker = directory / "efs-mock"
    subprocess.run(["cc", "-std=c11", "-D_POSIX_C_SOURCE=200809L", "-O2", "-Wall", "-Wextra", "-Werror",
                    "-DDIAG_EFS_TEST", str(ROOT / "rust/efs_worker.c"),
                    str(ROOT / "tests/efs_mock/mock_diag.c"), "-o", str(worker)], check=True)
    data_dir = directory / "data"
    data_dir.mkdir(mode=0o700)
    token_file = directory / "token"
    token_file.write_text(TOKEN)
    token_file.chmod(0o600)
    config = directory / "scenario"
    config.write_text("")
    trace = directory / "trace"
    ubus = directory / "ubus"
    ubus.write_text("""#!/usr/bin/env python3
import json
import sys
valid = False
if len(sys.argv) == 5 and sys.argv[1:4] == ["call", "zwrt_web", "webtoken_check"]:
    args = json.loads(sys.argv[4])
    valid = args.get("webtoken") == "efs-web-session-fixture" and args.get("web_remote_addr") == "127.0.0.1"
print(json.dumps({"ok": valid}))
""")
    ubus.chmod(0o700)
    local, lan = free_port(), free_port()
    env = dict(os.environ, ZWRT_DATAD_DIR=str(data_dir),
               ZWRT_DATAD_UBUS_BIN=str(ubus), ZWRT_DATAD_UCI_BIN="/usr/bin/false",
               ZWRT_DATAD_OTA_DISABLE_AUTO="1", ZWRT_DATAD_EFS_TEST_WORKER=str(worker),
               ZWRT_DATAD_EFS_TEST_CONFIG=str(config), MOCK_TRACE=str(trace), MOCK_GUARD_TREE="1")
    proc = subprocess.Popen([BIN, "-p", str(local), "--lan-bind", "127.0.0.1",
                             "--lan-port", str(lan), "--auth-token-file", str(token_file)],
                            env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        deadline = time.monotonic() + 15
        while True:
            try:
                if request(lan, "/healthz")[0] == 200:
                    break
            except OSError:
                pass
            assert proc.poll() is None and time.monotonic() < deadline, "startup failed"
            time.sleep(0.1)

        routes = [("/efs/status", "GET"), ("/efs/list?path=/", "GET"), ("/efs/nvread?item=550", "GET"),
                  ("/efs/stat?path=/nvefs/file", "GET"), ("/efs/read?path=/nvefs/file", "GET"),
                  ("/efs/download?path=/nvefs/file", "GET"), ("/efs/write", "POST"),
                  ("/efs/upload?path=/nvefs/file", "POST"), ("/efs/delete", "POST"),
                  ("/efs/mkdir", "POST"), ("/efs/rename", "POST")]
        bad_auth = [{}, {"Authorization":"Bearer wrong"}, {"Authorization":"Basic " + TOKEN},
                    {"X-Auth-Token":TOKEN}, {"Cookie":"access_token=" + TOKEN}]
        for port in (local, lan):
            for path, method in routes:
                for headers in bad_auth:
                    status, _, body = request(port, path, method, b'not valid JSON', headers)
                    assert status == 401, (port, path, status)
                    assert TOKEN.encode() not in body
                query_token = path + ("&" if "?" in path else "?") + "access_token=" + TOKEN
                assert request(port, query_token, method, b"{}", {})[0] == 401
            assert request(port, "/efs/status", headers=AUTH)[0] == 200
        assert not trace.exists(), "unauthenticated requests invoked the worker"

        # Obtain an actual datad session through its existing auth endpoint.
        # The fixture replaces only the device's webtoken verifier.
        assert request(lan, "/auth/exchange", "POST", headers={"X-Web-Token":"wrong"})[0] == 401
        status, _, raw = request(lan, "/auth/exchange", "POST",
                                 headers={"X-Web-Token":"efs-web-session-fixture"})
        assert status == 200, raw
        session = json.loads(raw)["access_token"]
        session_auth = {"Authorization":"Bearer " + session}
        for port in (local, lan):
            assert request(port, "/efs/status", headers=session_auth)[0] == 200
        assert not trace.exists(), "capability query accessed the modem"

        status, headers, _ = request(lan, "/efs/delete", "OPTIONS", headers={
            "Origin":"http://192.168.0.2:2333", "Access-Control-Request-Method":"POST",
            "Access-Control-Request-Headers":"authorization,content-type"})
        assert status == 204 and headers.get("access-control-allow-origin") == "http://192.168.0.2:2333"
        assert not trace.exists(), "browser preflight invoked the worker"

        def call(path, method="GET", body=None, expected=200):
            headers = dict(AUTH)
            if isinstance(body, dict):
                headers["Content-Type"] = "application/json"
                body = json.dumps(body).encode()
            status, response_headers, raw = request(lan, path, method, body, headers)
            assert status == expected, (path, status, raw)
            assert response_headers.get("cache-control") == "no-store"
            if response_headers.get("content-type", "").startswith("application/octet-stream"):
                return raw
            obj = json.loads(raw)
            assert obj["ok"] == (expected == 200), obj
            return obj

        for port in (local, lan):
            assert request(port, "/efs/nvread?item=550")[0] == 401
            assert request(port, "/efs/nvread?item=550&access_token="+TOKEN)[0] == 401
            assert request(port, "/efs/nvread?item=550", headers=session_auth)[0] == 200
        imei = call("/efs/nvread?item=550")["data"]
        assert imei == {"encoding":"hex","bytes":133,"hex":"262602084a09512430325781" + "00"*121}
        assert call("/efs/nvread?item=551")["data"]["hex"].startswith("262702")
        config.write_text("nv_status")
        assert bytes.fromhex(call("/efs/nvread?item=550")["data"]["hex"])[131] == 5
        config.write_text("")

        before = trace.read_text() if trace.exists() else ""
        for path, method, body in [
            ("/efs/nvread?item=65536", "GET", None),
            ("/efs/nvread?item=-1", "GET", None),
            ("/efs/nvread", "GET", None),
            ("/efs/nvread?item=550&command=raw", "GET", None),
            ("/efs/list?path=relative", "GET", None),
            ("/efs/list?path=/&command=raw", "GET", None),
            ("/efs/read?path=/nvefs/file&length=65537", "GET", None),
            ("/efs/write", "POST", {"path":"/nvefs/file", "hex":"xx"}),
            ("/efs/write", "POST", {"path":"/nvefs/file", "hex":"01", "local_path":"/etc/passwd"}),
            ("/efs/delete", "POST", {"path":"/nvefs/file", "directory":"yes"}),
            ("/efs/mkdir", "POST", {"path":"/nvefs/dir", "mode":"1000"}),
        ]:
            call(path, method, body, 400)
        assert (trace.read_text() if trace.exists() else "") == before

        before = trace.read_text() if trace.exists() else ""
        for port in (local, lan):
            for path in ("/", "/file", "/nvefs"):
                for route, payload in [("write", {"path":path,"hex":"00"}), ("delete", {"path":path}),
                                       ("delete", {"path":path,"directory":True}), ("mkdir", {"path":path}),
                                       ("rename", {"path":path,"destination":"/nvefs/new"}),
                                       ("rename", {"path":"/nvefs/file","destination":path})]:
                    h=dict(AUTH, **{"Content-Type":"application/json"})
                    assert request(port, "/efs/"+route, "POST", json.dumps(payload), h)[0]==403
                assert request(port, "/efs/upload?"+urlencode({"path":path}), "POST", b"x", AUTH)[0]==403
        assert (trace.read_text() if trace.exists() else "") == before
        for path in ("//file", "/nvefs/../file", "/nvefs/./file", "/nvefs//file", "/nvefs/file/"):
            call("/efs/write", "POST", {"path":path,"hex":"00"}, 400)
        for path in ("/nvefs/link", "/nvefs/link/file", "/nvefs/hard", "/nvefs/item"):
            before = trace.stat().st_size if trace.exists() else 0
            call("/efs/write", "POST", {"path":path,"hex":"00"}, 403)
            events=trace.read_text()[before:]
            assert "OPEN_FLAGS" not in events and "OP 5\n" not in events
        assert call("/efs/status")["safety"] == {"version":1,"root_readonly":True,"recursive_delete":False,"no_symlink_mutations":True}
        call("/efs/delete", "POST", {"path":"/nvefs/empty","recursive":True}, 400)
        before = trace.stat().st_size
        call("/efs/rename", "POST", {"path":"/nvefs/old","destination":"/nvefs/file"}, 403)
        assert "OP 14\n" not in trace.read_text()[before:], "rename must never overwrite an existing destination"

        assert request(lan, "/efs/stat?path=/nvefs/file", headers=session_auth)[0] == 200

        entries = call("/efs/list?path=/nvefs")["data"]["entries"]
        assert [e["name"] for e in entries] == ["file", "empty", "old", "link", "item", "hard"]
        assert call("/efs/stat?path=/nvefs/file")["data"]["size"] == 512
        data = call("/efs/read?path=/nvefs/file&length=35&offset=158")["data"]
        assert bytes.fromhex(data["hex"]) == CONTENT[158:193]
        tricky = "/nvefs/a $(id); b"
        call("/efs/list?" + urlencode({"path":tricky}))
        assert (tricky.encode() + b"\0").hex() in trace.read_text(), "path was interpreted by a shell"

        reply = call("/efs/write", "POST", {"path":"/nvefs/file", "hex":"00ff01", "offset":12})
        assert reply["command"] == "write" and reply["data"]["bytes"] == 3
        assert "local_path" not in reply["data"] and "OPEN_FLAGS 65" in trace.read_text()
        for path, body in [("/efs/delete", {"path":"/nvefs/file"}),
                           ("/efs/delete", {"path":"/nvefs/empty", "directory":True}),
                           ("/efs/mkdir", {"path":"/nvefs/dir", "mode":"0700"}),
                           ("/efs/rename", {"path":"/nvefs/old", "destination":"/nvefs/new"})]:
            call(path, "POST", body)
        uploaded = call("/efs/upload?path=/nvefs/file", "POST", CONTENT)
        assert uploaded["data"]["bytes"] == 512 and "local_path" not in uploaded["data"]
        assert call("/efs/download?path=/nvefs/file") == CONTENT
        assert sorted(p.name for p in (data_dir / "efs").iterdir()) == []

        for scenario, path, method, body, expected, code in [
            ("delete_error", "/efs/delete", "POST", {"path":"/nvefs/file"}, 403, "EFS_ERROR"),
            ("stat_error", "/efs/stat?path=/nvefs/file", "GET", None, 404, "EFS_ERROR"),
            ("timeout", "/efs/list?path=/", "GET", None, 504, "TIMEOUT"),
            ("wrong_header", "/efs/list?path=/", "GET", None, 502, "PROTOCOL_ERROR"),
            ("library_error", "/efs/list?path=/", "GET", None, 503, "LIBRARY_ERROR"),
            ("second_write_error", "/efs/upload?path=/nvefs/file", "POST", CONTENT, 507, "EFS_ERROR"),
        ]:
            config.write_text(scenario)
            obj = call(path, method, body, expected)
            assert obj["error"]["code"] == code
            if scenario == "second_write_error":
                assert obj["error"]["bytes_transferred"] == 150
        config.write_text("")
        assert request(lan, "/efs/upload?path=/nvefs/file", "POST", b"x" * (1024*1024+1), AUTH)[0] == 413
        assert request(lan, "/efs/raw", "POST", b"{}", AUTH)[0] == 404

        config.write_text("slow")
        with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
            before = trace.stat().st_size
            first = pool.submit(call, "/efs/mkdir", "POST", {"path":"/nvefs/slow"})
            deadline = time.monotonic()+5
            while trace.stat().st_size == before:
                assert time.monotonic() < deadline
                time.sleep(0.01)
            busy = call("/efs/delete", "POST", {"path":"/nvefs/file"}, 409)
            assert busy["error"]["code"] == "efs_busy"
            assert call("/efs/nvread?item=550", expected=409)["error"]["code"] == "efs_busy"
            first.result(timeout=10)
        config.write_text("")

        outside = directory / "outside"
        outside.mkdir(mode=0o700)
        (data_dir / "efs").rmdir()
        (data_dir / "efs").symlink_to(outside, target_is_directory=True)
        obj = call("/efs/list?path=/", expected=500)
        assert obj["error"]["code"] == "efs_storage_failed" and not list(outside.iterdir())
    finally:
        proc.terminate()
        proc.wait(timeout=10)

print("EFS: both-listener Bearer/session auth, browser preflight, validation, JSON/binary transfers, device errors, serialization and private storage verified")
