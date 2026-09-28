#!/usr/bin/env python3
"""Mock the current U50S GoAhead challenge, session and write contract."""
import hashlib
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

BINARY = sys.argv[1] if len(sys.argv) > 1 else "rust/target/debug/zwrt-datad"
PASSWORD = "test-only-password"
LD = "a" * 64
RD = "b" * 64

def digest(value):
    return hashlib.sha256(value.encode()).hexdigest()

class Vendor(BaseHTTPRequestHandler):
    writes = 0
    def send_json(self, value, cookie=None):
        data = json.dumps(value).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        if cookie:
            self.send_header("Set-Cookie", cookie)
        self.end_headers()
        self.wfile.write(data)
    def do_GET(self):
        if self.headers.get("Host") != "192.168.0.1" or self.headers.get("Referer") != "http://192.168.0.1/" or self.headers.get("X-Requested-With") != "XMLHttpRequest":
            return self.send_json({"network_type":""})
        keys = urllib.parse.parse_qs(urllib.parse.urlsplit(self.path).query).get("cmd", [""])[0].split(",")
        if keys == ["LD"]:
            return self.send_json({"LD": LD}, "pre=one; Path=/")
        if keys == ["loginfo"]:
            return self.send_json({"loginfo":"ok" if "sid=two" in self.headers.get("Cookie", "") else ""})
        if keys == ["RD"]:
            return self.send_json({"RD": RD})
        values = {"model_name":"U50S","network_type":"LTE","wa_inner_version":"B02","cr_version":"","Language":"en"}
        reply = {key: values.get(key, "") for key in keys}
        page = urllib.parse.parse_qs(urllib.parse.urlsplit(self.path).query).get("page", [None])[0]
        if page is not None:
            reply["page_seen"] = page
        return self.send_json(reply)
    def do_POST(self):
        form = urllib.parse.parse_qs(self.rfile.read(int(self.headers.get("Content-Length", "0"))).decode())
        action = form.get("goformId", [""])[0]
        if self.headers.get("Host") != "192.168.0.1" or self.headers.get("Referer") != "http://192.168.0.1/" or self.headers.get("X-Requested-With") != "XMLHttpRequest":
            return self.send_json({"result":"failure"})
        if action == "LOGIN":
            expected = digest(digest(PASSWORD) + LD)
            if form.get("password", [""])[0] == expected:
                return self.send_json({"result":"0"}, "sid=two; Path=/")
            return self.send_json({"result":"3"})
        if action == "SET_WEB_LANGUAGE":
            if "sid=two" in self.headers.get("Cookie", "") and "AD" not in form and form.get("Language", [""])[0] == "en":
                Vendor.writes += 1
                return self.send_json({"result":"success"})
        if action == "SET_DEVICE_LED":
            expected = digest(digest("B02") + RD)
            if "sid=two" in self.headers.get("Cookie", "") and form.get("AD", [""])[0] == expected and form.get("night_mode_switch", [""])[0] == "0":
                Vendor.writes += 1
                return self.send_json({"result":"success"})
        return self.send_json({"result":"failure"})
    def log_message(self, *_args):
        pass

def request(url, body=None, token=None):
    headers = {"Content-Type":"application/json"}
    if token:
        headers["Authorization"] = "Bearer " + token
    data = json.dumps(body).encode() if body is not None else None
    try:
        with urllib.request.urlopen(urllib.request.Request(url, data=data, headers=headers), timeout=3) as response:
            return response.status, json.load(response)
    except urllib.error.HTTPError as error:
        return error.code, json.load(error)

with tempfile.TemporaryDirectory(prefix="u50-oem-test-") as tmp:
    cfg = Path(tmp) / "cfg"
    cfg.write_text('''#!/bin/sh
case "$1:$2" in
 get:model_name) echo U50S ;;
 get:lan_ipaddr) echo 192.168.0.1 ;;
 get:integrate_version) echo B02 ;;
 get:network_type) echo LTE ;;
 *) exit 1 ;;
esac
''')
    cfg.chmod(0o700)
    server = ThreadingHTTPServer(("127.0.0.1", 0), Vendor)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        port = probe.getsockname()[1]
    env = {**os.environ, "ZWRT_DATAD_U50_CFG_BIN":str(cfg)}
    process = subprocess.Popen([BINARY,"--u50-model","u50s","--u50-enable-writes","--u50-goform-url",f"http://127.0.0.1:{server.server_port}/goform/goform_get_cmd_process","--port",str(port)],env=env,stdout=subprocess.DEVNULL,stderr=subprocess.PIPE,text=True)
    base = f"http://127.0.0.1:{port}"
    try:
        for _ in range(40):
            try:
                status, caps = request(base + "/capabilities")
                assert status == 200
                break
            except urllib.error.URLError:
                time.sleep(0.1)
        else:
            raise AssertionError("candidate server did not start")
        assert caps["controls"] == ["u50.oem.goform"]
        action = {"action":"u50.oem.goform","goform_id":"SET_DEVICE_LED","params":{"night_mode_switch":"0"},"confirm":True}
        assert request(base + "/control", action)[0] == 401
        status, login = request(base + "/auth/login", {"password":PASSWORD})
        assert status == 200 and login["ok"]
        token = login["token"]
        status, read = request(base + "/oem/read?cmd=network_type", token=token)
        assert status == 200 and read["fields"]["network_type"] == "LTE"
        status, paged = request(base + "/oem/read?cmd=network_type&page=2", token=token)
        assert status == 200 and paged["fields"]["page_seen"] == "2"
        assert request(base + "/oem/read?cmd=network_type&bad%3Bkey=x", token=token)[0] == 400
        assert request(base + "/control", {**action,"goform_id":"NOT_ALLOWED"}, token)[0] == 400
        assert request(base + "/control", {**action,"params":{"AD":"injected"}}, token)[0] == 400
        assert request(base + "/control", {**action,"confirm":False}, token)[0] == 400
        status, result = request(base + "/control", action, token)
        assert status == 200 and result["ok"] and result["verified"] is False
        assert Vendor.writes == 1
        language_action = {"action":"u50.oem.goform","goform_id":"SET_WEB_LANGUAGE","params":{"Language":"en"},"confirm":True}
        status, language_result = request(base + "/control", language_action, token)
        assert status == 200 and language_result["verified"] is True
        assert Vendor.writes == 2
        assert request(base + "/auth/logout", {}, token)[0] == 200
        assert request(base + "/control", action, token)[0] == 401
        print("U50 OEM challenge and guarded write contract OK")
    finally:
        process.terminate()
        process.wait(timeout=5)
        server.shutdown()
        server.server_close()
