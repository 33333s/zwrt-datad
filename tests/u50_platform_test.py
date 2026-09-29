#!/usr/bin/env python3
"""U50S platform backend: the mainline runtime served by the OEM GoAhead API.

A fake U50S GoAhead implements the challenge login (`sha256(stored_hash + LD)`),
the per-write AD challenge, SMS paging and the connection/bearer/SIM actions.
The daemon must derive its own session from the stored admin hash, expose the
mainline control API for the mapped actions, and never leak the credential.
"""
import hashlib
import json
import os
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
from pathlib import Path

BINARY = sys.argv[1] if len(sys.argv) > 1 else "rust/target/debug/zwrt-datad"
LD = "a" * 64
RD = "b" * 64
VERSION = "B02"


def digest(value):
    return hashlib.sha256(value.encode()).hexdigest().upper()


ADMIN_HASH = digest("device-admin-password")
STORE = {
    "model_name": "U50S", "network_type": "LTE", "wa_inner_version": VERSION, "cr_version": "",
    "dial_mode": "manual_dial", "roam_setting_option": "off", "net_select": "4G_AND_5G",
    "simcard_active_slot": "1", "sms_unread_num": "1", "sms_dev_unread_num": "1",
    "sms_sim_unread_num": "0", "sms_nv_num_total": "1", "sms_sim_num_total": "0",
}
MESSAGES = [{"id": "7", "number": "10086", "content": "6D4B8BD5", "tag": "1",
             "date": "26,08,27,04,00,00,+,0"}]
LOG = []


class Vendor(BaseHTTPRequestHandler):
    logged_in = False

    def send_json(self, value, cookie=None):
        data = json.dumps(value).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        if cookie:
            self.send_header("Set-Cookie", cookie)
        self.end_headers()
        self.wfile.write(data)

    def trusted(self):
        return (self.headers.get("Host") == "192.168.0.1"
                and self.headers.get("Referer") == "http://192.168.0.1/"
                and self.headers.get("X-Requested-With") == "XMLHttpRequest")

    def do_GET(self):
        query = urllib.parse.parse_qs(urllib.parse.urlsplit(self.path).query)
        if not self.trusted():
            return self.send_json({"network_type": ""})
        keys = query.get("cmd", [""])[0].split(",")
        if keys == ["LD"]:
            return self.send_json({"LD": LD}, "pre=one; Path=/")
        if keys == ["loginfo"]:
            return self.send_json({"loginfo": "ok" if "sid=two" in self.headers.get("Cookie", "") else ""})
        if keys == ["RD"]:
            return self.send_json({"RD": RD})
        if keys == ["sms_data_total"]:
            assert "multi_data" not in query, "the WebUI reads sms_data_total as a single command"
            assert query["order_by"] == ["order by id desc"] and query["tags"] == ["10"]
            store, page = query["mem_store"][0], query["page"][0]
            LOG.append(("sms_read", store, page, query["data_per_page"][0]))
            rows = MESSAGES if store == "1" and page == "0" else []
            return self.send_json({"messages": rows} if rows else {"sms_data_total": ""})
        if keys == ["sms_cmd_status_info"]:
            return self.send_json({"sms_cmd_status_result": "3"})
        return self.send_json({key: STORE.get(key, "") for key in keys})

    def do_POST(self):
        form = urllib.parse.parse_qs(self.rfile.read(int(self.headers.get("Content-Length", "0"))).decode())
        action = form.get("goformId", [""])[0]
        if not self.trusted():
            return self.send_json({"result": "failure"})
        if action == "LOGIN":
            if form.get("password", [""])[0] == digest(ADMIN_HASH + LD):
                LOG.append(("login",))
                return self.send_json({"result": "0"}, "sid=two; Path=/")
            return self.send_json({"result": "3"})
        authorized = ("sid=two" in self.headers.get("Cookie", "")
                      and form.get("AD", [""])[0] == digest(digest(VERSION + "") + RD))
        if not authorized:
            return self.send_json({"result": "failure"})
        one = {key: values[0] for key, values in form.items()}
        LOG.append(("write", action, {k: v for k, v in one.items() if k not in ("AD", "isTest")}))
        if action == "SET_CONNECTION_MODE":
            STORE["dial_mode"], STORE["roam_setting_option"] = one["ConnectionMode"], one["roam_setting_option"]
        elif action == "SET_BEARER_PREFERENCE":
            STORE["net_select"] = one["BearerPreference"]
        elif action == "SWITCH_SIMCARD_SLOT":
            STORE["simcard_active_slot"] = one["simcard_active_slot"]
        return self.send_json({"result": "success"})

    def log_message(self, *_args):
        pass


def call(port, path, body=None):
    data = json.dumps(body).encode() if body is not None else None
    request = urllib.request.Request(f"http://127.0.0.1:{port}{path}", data=data,
                                     headers={"Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(request, timeout=8) as response:
            return response.status, json.load(response)
    except urllib.error.HTTPError as error:
        return error.code, json.load(error)


def control(port, action, params):
    return call(port, "/control", {"action": action, "params": params})


def writes():
    return [entry for entry in LOG if entry[0] == "write"]


with tempfile.TemporaryDirectory(prefix="u50-platform-test-") as tmp:
    folder = Path(tmp)
    cfg = folder / "cfg"
    cfg.write_text(f'''#!/bin/sh
case "$1:$2" in
  get:model_name) echo U50S ;;
  get:lan_ipaddr) echo 192.168.0.1 ;;
  get:integrate_version) echo {VERSION} ;;
  get:network_type) echo LTE ;;
  get:roam_setting_option) echo off ;;
  get:admin_Password) echo {ADMIN_HASH} ;;
  *) exit 1 ;;
esac
''')
    cfg.chmod(0o700)
    server = ThreadingHTTPServer(("127.0.0.1", 0), Vendor)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        port = probe.getsockname()[1]
    (folder / "data").mkdir()
    # An enrolled U50S: remote access on, no proxied back-ends (the mainline
    # validator used to reject this combination).
    cloud_file = folder / "data" / "cloud.json"
    cloud_file.write_text(json.dumps({
        "enabled": True, "remote_enabled": True, "broker": "wss://127.0.0.1:1/mqtt",
        "platform_url": "https://nms.example.com", "username": "fixture-user",
        "password": "fixture-password", "model": "U50S", "identity": "fixture-device", "services": [],
        "remote_webshell_enabled": True}))
    cloud_file.chmod(0o600)
    url = f"http://127.0.0.1:{server.server_port}/goform/goform_get_cmd_process"
    env = {**os.environ, "ZWRT_DATAD_U50_CFG_BIN": str(cfg), "ZWRT_DATAD_OTA_DISABLE_AUTO": "1"}
    process = subprocess.Popen(
        [BINARY, "--u50-model", "u50s", "--u50-goform-url", url, "--u50-data-dir", str(folder / "data"),
         "--port", str(port)], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
    try:
        for _ in range(60):
            try:
                if call(port, "/capabilities")[0] == 200:
                    break
            except urllib.error.URLError:
                time.sleep(0.1)
        else:
            raise AssertionError("U50 runtime did not start")

        # Mainline API surface, restricted to what the U50S implements.
        _, caps = call(port, "/capabilities")
        assert "cellular.set" in caps["controls"] and "sms.send_raw" in caps["controls"]
        assert not any(name.startswith("speedtest") or name.startswith("wifi") for name in caps["controls"])

        _, cloud = call(port, "/cloud/config")
        assert cloud["config"]["enabled"] is True and cloud["config"]["remote_enabled"] is True, cloud
        assert cloud["config"]["services"] == [] and cloud["config"]["remote_webshell_enabled"] is True

        # SMS: counters plus paged, decoded messages in the mainline shape.
        for _ in range(60):
            _, state = call(port, "/state")
            if state.get("sms", {}).get("list"):
                break
            time.sleep(0.2)
        sms = state["sms"]
        assert sms["unread"] == 1 and sms["stale"] is False, sms
        assert sms["list"][0]["text"] == "测试" and sms["list"][0]["num"] == "10086" and sms["list"][0]["unread"] == 1
        assert ("login",) in LOG, "the daemon must open its own OEM session from the stored hash"
        assert state["net"]["roaming_allowed"] == 0

        # Roaming: the current dial mode is preserved, the result is read back.
        status, result = control(port, "cellular.set", {"roaming": 1})
        assert status == 200 and result["result"] == {"roaming": True, "verified": True}, result
        assert writes()[-1] == ("write", "SET_CONNECTION_MODE",
                                {"goformId": "SET_CONNECTION_MODE", "ConnectionMode": "manual_dial",
                                 "roam_setting_option": "on"})
        assert control(port, "cellular.set", {"roaming": 2})[0] == 400
        assert control(port, "cellular.set", {})[0] == 400
        assert control(port, "cellular.set", {"connect_mode": "sometimes"})[0] == 400

        status, result = control(port, "network.set_mode", {"mode": "Only_5G"})
        assert status == 200 and result["result"] == {"mode": "Only_5G", "verified": True}, result
        assert control(port, "network.set_mode", {"mode": "rm -rf"})[0] == 400
        status, result = control(port, "sim.set_slot", {"slot": 2})
        assert status == 200 and result["result"]["verified"] is True
        assert control(port, "sim.set_slot", {"slot": 3})[0] == 400

        # SMS actions use the OEM list format and the WebUI's message encoding.
        status, result = control(port, "sms.send_raw", {
            "sender": "host", "number": "+8613800000000", "message_hex": "6D4B8BD5",
            "sms_time": "26;08;27;04;00;00;+;0"})
        assert status == 200 and result["result"]["status"] == 3, result
        assert writes()[-1] == ("write", "SEND_SMS", {
            "goformId": "SEND_SMS", "Number": "+8613800000000", "sms_time": "26;08;27;04;00;00;+;0",
            "MessageBody": "6D4B8BD5", "ID": "-1", "encode_type": "UNICODE"})
        assert control(port, "sms.send_raw", {"sender": "host", "number": "1;reboot", "message_hex": "6D4B8BD5",
                                              "sms_time": "26;08;27;04;00;00;+;0"})[0] == 400
        status, _ = control(port, "sms.delete", {"ids": [7, "8"]})
        assert status == 200 and writes()[-1][2]["msg_id"] == "7;8;"
        assert control(port, "sms.delete", {"ids": ["7;reboot"]})[0] == 400
        status, _ = control(port, "sms.mark_read", {"ids": "7", "tag": 0})
        assert status == 200 and writes()[-1][2] == {"goformId": "SET_MSG_READ", "msg_id": "7;", "tag": "0"}
        assert control(port, "sms.mark_read", {"ids": "7", "tag": 5})[0] == 400

        # Device actions and unmapped actions.
        assert control(port, "device.reboot", {})[0] == 200 and writes()[-1][1] == "REBOOT_DEVICE"
        assert control(port, "wifi.configure", {"section": "main_2g"})[0] == 404
        assert control(port, "cooling.fan.set_mode", {"mode": "custom"})[0] == 404

        # The credential never appears in any reply.
        _, final_state = call(port, "/state")
        assert ADMIN_HASH not in json.dumps(final_state) and "device-admin-password" not in json.dumps(final_state)
    finally:
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
        server.shutdown()
print("U50 platform: local OEM session, SMS, roaming, bearer, SIM and device controls OK")
