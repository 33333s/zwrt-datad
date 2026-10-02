#!/usr/bin/env python3
"""U50S platform backend: the mainline runtime served by the OEM GoAhead API.

A fake U50S GoAhead implements the challenge login (`sha256(stored_hash + LD)`),
the per-write AD challenge, SMS paging and the connection/bearer/SIM actions.
The daemon must derive its own session from the stored admin hash, expose the
mainline control API for the mapped actions, and never leak the credential.
"""
import concurrent.futures
import base64
import http.client
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
    "lte_band_lock": "0x1c200000095", "lte_band_1_64_factory":"0x1c200000095", "nr5g_sa_band_factory":"1,3,5,8,28,41,77,78", "nr5g_nsa_band_factory":"1,3,5,8,28,41,77,78", "nr5g_sa_band_lock": "5,7,78", "nr5g_nsa_band_lock": "5,7,78",
    "data_volume_limit_switch": "0", "data_volume_limit_unit": "", "data_volume_limit_size": "",
    "data_volume_alert_percent": "", "wan_auto_clear_flow_data_switch": "on", "traffic_clear_date": "1",
    "flux_limited_disconnect": "off",
    "lan_ipaddr": "192.168.0.1", "lan_netmask": "255.255.255.0", "dhcpEnabled": "1", "dhcpStart": "192.168.0.2",
    "dhcpEnd": "192.168.0.253", "dhcpLease_hour": "24", "mtu": "1500", "tcp_mss": "1460", "wifi_onoff_state": "1",
}
MESSAGES = [{"id": "7", "number": "10086", "content": "6D4B8BD5", "tag": "1",
             "date": "26,08,27,04,00,00,+,0"}]
APN_RECORD = "Fixture($)fixture.apn($)unused($)unused($)PAP($)apn-account-secret($)apn-password-secret($)IP($)0($)0($)auto($)($)"
STORE.update({"apn_mode": "manual", "apn_interface_version": "2", "profile_name_ui": "Fixture",
              "wan_apn_ui": "fixture.apn", "APN_config0": APN_RECORD, "apn_auto_config": APN_RECORD,
              "wifi_lbd_enable": "0"})
# Neighbor rows straight from the firmware: `;`-separated cells. The middle LTE
# row is malformed and must be dropped by the parser.
STORE.update({"lte_ngbr_cell_info_ext": "3725,57,-13,-96,3;x,1,2,3,4;3650,973,-11,-101,28;",
              "sa_ngbr_cell_manual_result_ext": "973,627264,-103,-14,n78;",
              "m_netselect_status": ""})
WIFI_POINTS = {"ResponseList": [
    {"ChipIndex": "0", "AccessPointIndex": "0", "Band": "b", "SSID": "fixture-24", "AuthMode": "WPA2PSK",
     "Password": base64.b64encode(b"wifi-password-secret").decode(), "ApIsolate": "0",
     "ApBroadcastDisabled": "0", "Pmf_switch": "1",
     "AccessPointSwitchStatus": "1", "ApMaxStationNumber": "16", "CountryCode": "CN", "Channel": "11", "BandWidth": "1"},
    {"ChipIndex": "1", "AccessPointIndex": "0", "Band": "a", "SSID": "fixture-5", "AuthMode": "WPA3PSK",
     "Password": base64.b64encode(b"wifi-password-secret").decode(), "ApIsolate": "0",
     "ApBroadcastDisabled": "1", "Pmf_switch": "2",
     "AccessPointSwitchStatus": "1", "ApMaxStationNumber": "16", "CountryCode": "CN", "Channel": "149", "BandWidth": "4"},
    {"ChipIndex": "0", "AccessPointIndex": "1", "Band": "b", "SSID": "fixture-guest", "AuthMode": "OPEN",
     "Password": "", "ApIsolate": "0", "ApBroadcastDisabled": "0", "Pmf_switch": "0",
     "AccessPointSwitchStatus": "0", "ApMaxStationNumber": "16", "CountryCode": "CN", "Channel": "11", "BandWidth": "1"},
]}
CHIP_ADVANCED = {"ResponseList": [
    {"ChipIndex": "0", "CountryCode": "CN", "Channel": "0", "BandWidth": "2", "Band": "b", "WirelessMode": "4"},
    {"ChipIndex": "1", "CountryCode": "CN", "Channel": "0", "BandWidth": "6", "Band": "a", "WirelessMode": "6"},
]}
LOG = []


class Vendor(BaseHTTPRequestHandler):
    logged_in = False
    developer = False
    skip_mode_write = False
    delay_mode_write = False
    pending_mode = None
    rotate_rd = False
    rd_counter = 0
    current_rd = RD

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
        if getattr(Vendor, "empty_apn", False) and ("apn_mode" in keys or any("APN_config" in key for key in keys)):
            return self.send_json({key: "" for key in keys})
        if Vendor.pending_mode and time.monotonic() >= Vendor.pending_mode[1]:
            STORE["net_select"] = Vendor.pending_mode[0]
            Vendor.pending_mode = None
        if keys == ["LD"]:
            return self.send_json({"LD": LD}, "pre=one; Path=/")
        if keys == ["loginfo"]:
            return self.send_json({"loginfo": "ok" if "sid=two" in self.headers.get("Cookie", "") else ""})
        if keys == ["RD"]:
            if Vendor.rotate_rd:
                Vendor.rd_counter += 1
                Vendor.current_rd = digest(f"challenge-{Vendor.rd_counter}")
                current = Vendor.current_rd
                time.sleep(0.1)
            else:
                Vendor.current_rd = current = RD
            return self.send_json({"RD": current})
        if keys == ["developer_option_loginfo"]:
            return self.send_json({"developer_option_loginfo": "ok" if Vendor.developer else ""})
        if keys == ["queryWiFiChipAdvancedInfo"]:
            assert "multi_data" not in query
            assert "sid=two" in self.headers.get("Cookie", "")
            return self.send_json(CHIP_ADVANCED)
        if keys in (["queryAccessPointInfo"], ["queryWiFiModuleSwitch"]):
            assert "multi_data" not in query, "Wi-Fi resources must use the WebUI single-command reads"
            assert "sid=two" in self.headers.get("Cookie", ""), "configuration must use the local OEM session"
            LOG.append(("configuration_read", keys[0]))
            return self.send_json(WIFI_POINTS if keys == ["queryAccessPointInfo"] else {"WiFiModuleSwitch": "1"})
        if any(key.startswith("APN_config") or key.startswith("ipv6_APN_config") for key in keys):
            assert query.get("multi_data") == ["1"] and len(keys) <= 32
            assert "sid=two" in self.headers.get("Cookie", "")
            LOG.append(("configuration_read", "apn_profiles"))

        if keys == ["sms_data_total"]:
            assert "multi_data" not in query, "the WebUI reads sms_data_total as a single command"
            assert query["order_by"] == ["order by id desc"] and query["tags"] == ["10"]
            store, page = query["mem_store"][0], query["page"][0]
            LOG.append(("sms_read", store, page, query["data_per_page"][0]))
            rows = MESSAGES if store == "1" and page == "0" else []
            return self.send_json({"messages": rows} if rows else {"sms_data_total": ""})
        if keys == ["station_list"]:
            return self.send_json({"station_list": [
                {"mac_addr": "AA:BB:CC:00:00:01", "hostname": "phone", "ip_addr": "192.168.0.10", "ssid_index": "0"}]})
        if keys == ["lan_station_list"]:
            return self.send_json({"station_list": [
                {"mac_addr": "AA:BB:CC:00:00:02", "hostname": "desk", "ip_addr": "192.168.0.11"}]})
        if keys == ["queryDeviceAccessControlList"]:
            return self.send_json({"AclMode": STORE.get("AclMode", "2"), "BlackMacList": STORE.get("BlackMacList", ""),
                                   "BlackNameList": STORE.get("BlackNameList", ""), "WhiteMacList": "", "WhiteNameList": ""})
        if keys == ["sms_cmd_status_info"]:
            return self.send_json({"sms_cmd_status_result": "3"})
        return self.send_json({key: STORE.get(key, "") for key in keys})

    def do_POST(self):
        form = urllib.parse.parse_qs(self.rfile.read(int(self.headers.get("Content-Length", "0"))).decode(),
                                     keep_blank_values=True)
        action = form.get("goformId", [""])[0]
        if not self.trusted():
            return self.send_json({"result": "failure"})
        if action == "LOGIN":
            if form.get("password", [""])[0] == digest(ADMIN_HASH + LD):
                LOG.append(("login",))
                return self.send_json({"result": "0"}, "sid=two; Path=/")
            return self.send_json({"result": "3"})
        authorized = ("sid=two" in self.headers.get("Cookie", "")
                      and form.get("AD", [""])[0] == digest(digest(VERSION + "") + Vendor.current_rd))
        if not authorized:
            return self.send_json({"result": "failure"})
        if action == "DEVELOPER_OPTION_LOGIN":
            # The firmware gates hidden-page goforms behind this second login;
            # it reuses the stored admin hash with a fresh LD challenge and
            # answers "0" (not "success") when the proof is accepted.
            if form.get("password", [""])[0] != digest(ADMIN_HASH + LD):
                return self.send_json({"result": "failure"})
            Vendor.developer = True
            LOG.append(("write", action, {k: v for k, v in {key: values[0] for key, values in form.items()}.items()
                                          if k not in ("AD", "isTest")}))
            return self.send_json({"result": "0"})
        if action in ("BAND_SELECT", "WAN_PERFORM_NR5G_BAND_LOCK",
                      "WAN_PERFORM_NR5G_SANSA_BAND_LOCK", "SCAN_NR5G_NEIGHBOR_CELL") and not Vendor.developer:
            return self.send_json({"result": "failure"})
        if action == "SCAN_NR5G_NEIGHBOR_CELL":
            # The firmware answers the scan request, then flips
            # m_netselect_status when the manual search finishes.
            STORE["m_netselect_status"] = "manual_selected"
            STORE["sa_ngbr_cell_manual_result_ext"] = "973,627264,-103,-14,n78;361,633984,-97,-7,n41;"
        one = {key: values[0] for key, values in form.items()}
        LOG.append(("write", action, {k: v for k, v in one.items() if k not in ("AD", "isTest")}))
        if action == "SET_CONNECTION_MODE":
            STORE["dial_mode"], STORE["roam_setting_option"] = one["ConnectionMode"], one["roam_setting_option"]
        elif action == "SET_BEARER_PREFERENCE":
            # The U50 Pro firmware reports the 4G+5G preference as WL_AND_5G.
            stored = ("WL_AND_5G" if one["BearerPreference"] == "4G_AND_5G"
                      else one["BearerPreference"])
            if Vendor.delay_mode_write:
                Vendor.pending_mode = (stored, time.monotonic() + 0.5)
            elif not Vendor.skip_mode_write:
                STORE["net_select"] = stored
        elif action == "SWITCH_SIMCARD_SLOT":
            STORE["simcard_active_slot"] = one["simcard_active_slot"]
        elif action == "BAND_SELECT":
            STORE["lte_band_lock"] = one["lte_band_mask"]
        elif action == "SET_NETWORK_BAND_LOCK":
            STORE["lte_band_lock"] = one["lte_band_lock"]
        elif action == "WAN_PERFORM_NR5G_SANSA_BAND_LOCK":
            STORE["nr5g_nsa_band_lock" if one["type"] == "1" else "nr5g_sa_band_lock"] = one["nr5g_band_mask"]
        elif action == "setDeviceAccessControlList":
            STORE["AclMode"], STORE["BlackMacList"], STORE["BlackNameList"] = one["AclMode"], one["BlackMacList"], one["BlackNameList"]
        elif action == "DHCP_SETTING":
            STORE["dhcpEnabled"] = "1" if one["lanDhcpType"] == "SERVER" else "0"
            for src, dst in (("dhcpStart", "dhcpStart"), ("dhcpEnd", "dhcpEnd"), ("dhcpLease", "dhcpLease_hour")):
                if src in one:
                    STORE[dst] = one[src]
        elif action == "SET_DEVICE_MTU":
            STORE["mtu"], STORE["tcp_mss"] = one["mtu"], one["tcp_mss"]
        elif action == "SET_WIFI_INFO" or action == "switchWiFiModule":
            STORE["wifi_onoff_state"] = one.get("wifiEnabled", one.get("SwitchOption", "1"))
            if "wifi_lbd_enable" in one:
                STORE["wifi_lbd_enable"] = one["wifi_lbd_enable"]
        elif action == "setWiFiChipAdvancedInfo":
            row = next(row for row in CHIP_ADVANCED["ResponseList"]
                       if row["ChipIndex"] == one.get("ChipIndex"))
            for key in ("Channel", "BandWidth", "WirelessMode", "CountryCode"):
                if key in one:
                    row[key] = one[key]
        elif action == "setAccessPointInfo":
            row = next(row for row in WIFI_POINTS["ResponseList"]
                       if row["ChipIndex"] == one.get("ChipIndex")
                       and row["AccessPointIndex"] == one.get("AccessPointIndex"))
            for key in ("SSID", "AuthMode", "EncrypType", "ApBroadcastDisabled", "ApIsolate",
                        "Pmf_switch", "ApMaxStationNumber", "AccessPointSwitchStatus"):
                if key in one:
                    row[key] = one[key]
            row["Password"] = one.get("Password", row["Password"])
        elif action == "DATA_LIMIT_SETTING":
            for key in ("data_volume_limit_switch", "data_volume_limit_unit", "data_volume_limit_size",
                        "data_volume_alert_percent", "wan_auto_clear_flow_data_switch", "traffic_clear_date"):
                if key in one:
                    STORE[key] = one[key]
        return self.send_json({"result": "success"})

    def log_message(self, *_args):
        pass


def call(port, path, body=None, headers=None):
    data = json.dumps(body).encode() if body is not None else None
    request = urllib.request.Request(f"http://127.0.0.1:{port}{path}", data=data,
                                     headers={"Content-Type": "application/json", **(headers or {})})
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
    cfg_mode = folder / "cfg-apn-mode"
    cfg_apn = folder / "cfg-apn-name"
    cfg_keys = folder / "cfg-keys"
    cfg_mode.write_text("auto\n")
    cfg_apn.write_text("cfg.fixture.apn\n")
    cfg.write_text(f'''#!/bin/sh
printf '%s\\n' "$1:$2" >> "{cfg_keys}"
case "$1:$2" in
  get:model_name) echo U50S ;;
  get:lan_ipaddr) echo 192.168.0.1 ;;
  get:integrate_version) echo {VERSION} ;;
  get:network_type) echo LTE ;;
  get:roam_setting_option) echo off ;;
  get:data_volume_limit_switch) echo 0 ;;
  get:wan_auto_clear_flow_data_switch) echo on ;;
  get:traffic_clear_date) echo 1 ;;
  get:dhcpLease_hour) echo 24 ;;
  get:admin_Password) echo {ADMIN_HASH} ;;
  get:apn_mode) cat "{cfg_mode}" ;;
  get:apn_interface_version) echo 2 ;;
  get:profile_name_ui) echo CfgFixture ;;
  get:wan_apn_ui) cat "{cfg_apn}" ;;
  get:ppp_auth_mode_ui) echo NONE ;;
  get:pdp_type_ui) echo IPv4v6 ;;
  *) exit 1 ;;
esac
''')
    cfg.chmod(0o700)
    server = ThreadingHTTPServer(("127.0.0.1", 0), Vendor)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        port = probe.getsockname()[1]
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        lan_port = probe.getsockname()[1]
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
         "--port", str(port), "--lan-port", str(lan_port)], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
    try:
        for _ in range(60):
            try:
                if call(port, "/capabilities")[0] == 200:
                    break
            except urllib.error.URLError:
                time.sleep(0.1)
        else:
            raise AssertionError("U50 runtime did not start")

        # The U50 LAN listener uses original WebUI credentials, never ubus.
        origin = "http://192.168.0.2:2333"
        connection = http.client.HTTPConnection("127.0.0.1", lan_port, timeout=8)
        connection.request("OPTIONS", "/auth/login", headers={"Origin": origin,
            "Access-Control-Request-Method": "POST", "Access-Control-Request-Headers": "authorization",
            "Access-Control-Request-Private-Network": "true"})
        response = connection.getresponse()
        assert response.status == 204
        assert response.getheader("Access-Control-Allow-Origin") == origin
        assert response.getheader("Access-Control-Allow-Private-Network") == "true"
        response.read()
        connection.close()
        assert call(lan_port, "/state")[0] == 401
        for user, password in [("admin", "wrong"), ("other", "device-admin-password")]:
            basic = base64.b64encode(f"{user}:{password}".encode()).decode()
            assert call(lan_port, "/auth/login", {}, {"Authorization": f"Basic {basic}"})[0] == 401
        basic = base64.b64encode(b"admin:device-admin-password").decode()
        status, login = call(lan_port, "/auth/login", {}, {"Authorization": f"Basic {basic}"})
        assert status == 200 and login["access_token"], login
        token = login["access_token"]
        assert call(lan_port, "/state", headers={"Authorization": f"Bearer {token}"})[0] == 200
        connection = http.client.HTTPConnection("127.0.0.1", lan_port, timeout=8)
        connection.request("GET", f"/events?access_token={token}", headers={"Origin": origin})
        response = connection.getresponse()
        assert response.status == 200 and response.getheader("Access-Control-Allow-Origin") == origin
        assert response.readline().startswith(b"event: state")
        connection.close()

        # Mainline API surface, restricted to what the U50S implements.
        _, caps = call(port, "/capabilities")
        assert "cellular.set" in caps["controls"] and "sms.send_raw" in caps["controls"]
        assert not any(name.startswith("speedtest") for name in caps["controls"])
        for action in ("wifi.status", "wifi.dual_band_status", "apn.list", "client.access",
                       "wifi.configure", "wifi.set_dual_band", "neighbor.set", "neighbor.status"):
            assert action in caps["controls"]
        assert "apn.add" not in caps["controls"]

        # Local reads use OEM resources, make no setting writes, and exclude
        # keys embedded in the firmware's APN response objects.
        before = len(writes())
        status, wifi = control(port, "wifi.status", {})
        assert status == 200, wifi
        assert wifi["result"]["main_2g"]["ssid"] == "fixture-24"
        assert wifi["result"]["main_2g"]["key"] == "wifi-password-secret"
        assert wifi["result"]["main_2g"]["isolate"] == "0"
        assert wifi["result"]["main_2g"]["writable"] is True
        assert wifi["result"]["main_5g"]["encryption"] == "sae"
        assert wifi["result"]["guest_2g"]["encryption"] == "none"
        assert wifi["result"]["guest_2g"]["disabled"] == "1"
        status, dual = control(port, "wifi.dual_band_status", {})
        assert status == 200 and dual["result"]["enabled"] is False and dual["result"]["writable"] is False
        status, apn = control(port, "apn.list", {})
        assert status == 200, apn
        assert apn["result"]["mode"] == {"apn_mode": 1} and apn["result"]["writable"] is False
        assert apn["result"]["enabled"] == {"profileId": "manual-0"}
        assert apn["result"]["manual"]["apnListArray"][0]["wanapn"] == "fixture.apn"
        # The real original image can return empty APN metadata. Fall back
        # only to reviewed cfg scalars, never raw profiles or credentials.
        Vendor.empty_apn = True
        status, cfg_view = control(port, "apn.list", {})
        assert status == 200, cfg_view
        assert cfg_view["result"]["mode"] == {"apn_mode": 0}
        assert cfg_view["result"]["enabled"] == {"profileId": "current"}
        assert cfg_view["result"]["writable"] is False
        assert cfg_view["result"]["automatic"]["apnListArray"][0]["wanapn"] == "cfg.fixture.apn"
        assert cfg_view["result"]["manual"]["apnListArray"] == []
        assert "get:APN_config" not in cfg_keys.read_text() and "get:ppp_passwd" not in cfg_keys.read_text()
        cfg_mode.write_text("invalid\n")
        assert control(port, "apn.list", {})[0] == 502
        cfg_mode.write_text("auto\n")
        cfg_apn.write_text("a" * 300)
        assert control(port, "apn.list", {})[0] == 502
        cfg_apn.write_text("cfg.fixture.apn\n")
        Vendor.empty_apn = False
        status, clients_read = control(port, "client.access", {})
        assert status == 200 and clients_read["result"]["total"] == 2
        for action in ("wifi.status", "wifi.dual_band_status", "apn.list", "client.access"):
            assert control(port, action, {"arbitrary": "value"})[0] == 400
        assert len(writes()) == before, "read actions must not modify the device"
        # The Wi-Fi key IS returned by wifi.status, matching the mainline
        # contract the panel app is built against (same authenticated local
        # API). APN and admin credentials still never leak.
        assert "wifi-password-secret" not in json.dumps([dual, apn, cfg_view, clients_read])
        for secret in ("apn-account-secret", "apn-password-secret", ADMIN_HASH):
            assert secret not in json.dumps([wifi, dual, apn, cfg_view, clients_read])
        assert control(port, "apn.add", {"name": "fixture"})[0] == 404
        assert len(writes()) == before

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
        assert state["dhcp"]["leasetime"] == "24h"

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
        assert status == 200 and result["result"] == {"result":"success", "mode": "Only_5G", "verified": True}, result
        assert control(port, "network.set_mode", {"mode": "rm -rf"})[0] == 400
        # U50 Pro: the input alias maps to 4G_AND_5G and the WL_AND_5G readback
        # still counts as verified.
        status, result = control(port, "network.set_mode", {"mode": "WL_AND_5G"})
        assert status == 200 and result["result"] == {"result":"success", "mode": "4G_AND_5G", "verified": True}, result
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

        # Band locks: WebUI mask format, read back, invalid input refused.
        status, result = control(port, "band.set_lte", {"bands": "1,3,41"})
        assert status == 200 and result["result"]["mask"] == "0x0000010000000005" and result["result"]["verified"], result
        band_writes = [w for w in writes() if w[1] == "SET_NETWORK_BAND_LOCK"]
        assert band_writes and band_writes[-1][2] == {
            "goformId": "SET_NETWORK_BAND_LOCK", "lte_band_lock": "0x0000010000000005"}
        for bad in ({"bands": "1,x"}, {"bands": "0"}, {"bands": "65"}, {}):
            assert control(port, "band.set_lte", bad)[0] == 400, bad
        status, result = control(port, "band.set_nr_nsa", {"bands": "78,41,78"})
        assert status == 200 and result["result"] == {"result":"success", "bands": [41, 78], "verified": True}, result
        nr_writes = [w for w in writes() if w[1] == "WAN_PERFORM_NR5G_SANSA_BAND_LOCK"]
        assert nr_writes and nr_writes[-1][2] == {"goformId": "WAN_PERFORM_NR5G_SANSA_BAND_LOCK", "nr5g_band_mask": "41,78", "type": "1"}
        # The NR lock is developer-gated: the bridge must have elevated the
        # session (password proof from the stored admin hash) exactly once.
        dev_logins = [w for w in writes() if w[1] == "DEVELOPER_OPTION_LOGIN"]
        assert len(dev_logins) == 1, dev_logins
        status, result = control(port, "band.set_nr_sa", {"bands": "78"})
        nr_writes = [w for w in writes() if w[1] == "WAN_PERFORM_NR5G_SANSA_BAND_LOCK"]
        assert status == 200 and nr_writes[-1][2]["type"] == "0"
        assert len([w for w in writes() if w[1] == "DEVELOPER_OPTION_LOGIN"]) == 1

        # CSV/array parity, auto-clear semantics and common mode names: an
        # empty list clears the lock (the WebUI's deselect-all sends mask
        # "0"), it never means "lock every factory band".
        assert control(port, "band.set_lte", {"bands": [1,3,41]})[0] == 200
        status, automatic = control(port, "band.set_lte", {"bands": ""})
        assert status == 200 and automatic["result"]["mode"] == "auto" and automatic["result"]["verified"], automatic
        # auto = factory bands (U50 Pro: mask 0 = no service, not auto)
        assert STORE["lte_band_lock"] == "0x000001c200000095"  # factory mask
        assert control(port, "band.set_nr_sa", {"bands": []})[0] == 200
        assert STORE["nr5g_sa_band_lock"] == "1,3,5,8,28,41,77,78"  # factory list
        # Bands outside the modem's factory tables are refused before any
        # write reaches the firmware (it would store them verbatim and the
        # modem would go offline hunting an unsupported band).
        assert control(port, "band.set_lte", {"bands": "1,19"})[0] == 400
        assert control(port, "band.set_nr_sa", {"bands": "79"})[0] == 400
        assert control(port, "network.set_mode", {"mode": "4G"})[1]["result"]["mode"] == "Only_LTE"
        assert control(port, "network.set_mode", {"mode": "auto"})[1]["result"]["mode"] == "4G_AND_5G"

        Vendor.rotate_rd = True
        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
            futures = [pool.submit(control, port, "band.set_lte", {"bands":[1,3,41]}),
                       pool.submit(control, port, "network.set_mode", {"mode":"4G"})]
            for future in futures:
                status, reply = future.result(timeout=10)
                assert status == 200 and reply["result"]["verified"] is True, reply
        Vendor.rotate_rd = False
        assert control(port, "network.set_mode", {"mode":"auto"})[0] == 200
        Vendor.delay_mode_write = True
        status, delayed = control(port, "network.set_mode", {"mode":"4G"})
        assert status == 200 and delayed["result"]["verified"] is True, delayed
        Vendor.delay_mode_write = False
        Vendor.skip_mode_write = True
        status, failed = control(port, "network.set_mode", {"mode":"5G"})
        assert status == 502 and failed["ok"] is False, failed
        Vendor.skip_mode_write = False

        # Cell locks use the WebUI's formats; unlock zeroes LTE and sends 1,1,1,1 for NR.
        assert control(port, "cell.lock_lte", {"pci": 57, "earfcn": 3725})[0] == 200
        assert writes()[-1][2] == {"goformId": "LTE_LOCK_CELL_SET", "lte_pci_lock": "57", "lte_earfcn_lock": "3725"}
        assert control(port, "cell.lock_nr", {"pci": 384, "arfcn": 633984, "band": 78})[0] == 200
        assert writes()[-1][2] == {"goformId": "NR5G_LOCK_CELL_SET", "nr5g_cell_lock": "384,633984,78,30"}
        assert control(port, "cell.lock_nr", {"pci": 5, "arfcn": 1, "band": 28, "scs": 15})[0] == 200
        assert writes()[-1][2]["nr5g_cell_lock"] == "5,1,28,15"
        for bad in ({"pci": 2000, "arfcn": 1, "band": 78}, {"pci": 1, "arfcn": 1, "band": 0}, {"pci": 1, "arfcn": 1, "band": 78, "scs": 5}):
            assert control(port, "cell.lock_nr", bad)[0] == 400, bad
        assert control(port, "cell.lock_lte", {"pci": "1;x", "earfcn": 1})[0] == 400
        assert control(port, "cell.unlock_all", {})[0] == 200
        assert writes()[-2][2] == {"goformId": "LTE_LOCK_CELL_SET", "lte_pci_lock": "0", "lte_earfcn_lock": "0"}
        assert writes()[-1][2] == {"goformId": "NR5G_LOCK_CELL_SET", "nr5g_cell_lock": "1,1,1,1"}

        # Neighbor cells: read-only list parses both firmwares' row shapes
        # (LTE and SA swap the pci/arfcn and rsrp/sinr positions) and drops
        # malformed rows.
        status, result = control(port, "neighbor.list", {})
        assert status == 200, result
        cells = result["result"]
        assert cells["network_type"] == "LTE" and cells["primary"] == "lte", cells
        assert cells["lte"] == [
            {"rat": "LTE", "pci": 57, "arfcn": 3725, "rsrp_dbm": -96, "sinr_db": -13, "band": 3},
            {"rat": "LTE", "pci": 973, "arfcn": 3650, "rsrp_dbm": -101, "sinr_db": -11, "band": 28}], cells
        assert cells["sa"] == [
            {"rat": "NR5G", "pci": 973, "arfcn": 627264, "rsrp_dbm": -103, "sinr_db": -14, "band": 78}], cells

        # SA neighbor scan: switch to Only_5G (the firmware precondition),
        # scan, poll m_netselect_status, harvest, and restore the mode —
        # whatever it was when the scan started.
        before_mode = STORE["net_select"]
        assert before_mode != "Only_5G"
        mark = len(writes())
        status, result = control(port, "neighbor.scan_sa", {})
        assert status == 200 and result["result"]["result"] == "success", result
        assert result["result"]["count"] == 2 and result["result"]["restored_mode"] == before_mode, result
        assert result["result"]["cells"][1] == {"rat": "NR5G", "pci": 361, "arfcn": 633984,
                                                "rsrp_dbm": -97, "sinr_db": -7, "band": 41}, result
        scan_writes = [w for w in writes()[mark:] if w[1] in ("SCAN_NR5G_NEIGHBOR_CELL", "SET_BEARER_PREFERENCE")]
        assert [w[1] for w in scan_writes] == ["SET_BEARER_PREFERENCE", "SCAN_NR5G_NEIGHBOR_CELL",
                                               "SET_BEARER_PREFERENCE"], scan_writes
        assert scan_writes[0][2]["BearerPreference"] == "Only_5G"
        assert scan_writes[2][2]["BearerPreference"] == before_mode
        assert STORE["net_select"] == before_mode

        # Wi-Fi settings on the mainline contract: configure with no-op
        # detection, switch-only saves, key handling and readback verify.
        status, result = control(port, "wifi.configure", {"section": "main_2g"})
        assert status == 400, result  # no fields
        mark = len(writes())
        saved = control(port, "wifi.status", {})[1]["result"]["main_2g"]
        saved.pop("disabled")  # the mainline contract also rejects it on save
        status, result = control(port, "wifi.configure", {"section": "main_2g", **{
            key: value for key, value in saved.items() if key != "writable"}})
        assert status == 200 and result["result"] == {"section": "main_2g", "changed": False, "verified": True}, result
        assert len(writes()) == mark
        status, result = control(port, "wifi.configure", {
            "section": "main_2g", "ssid": "renamed-24", "key": "new-password-8", "hidden": "1", "isolate": "1"})
        assert status == 200 and result["result"]["changed"] is True and result["result"]["verified"], result
        wifi_write = next(w for w in writes()[mark:] if w[1] == "setAccessPointInfo")
        assert wifi_write[2]["SSID"] == "renamed-24" and wifi_write[2]["AuthMode"] == "WPA2PSK"
        assert wifi_write[2]["EncrypType"] == "CCMP"
        assert wifi_write[2]["Password"] == base64.b64encode(b"new-password-8").decode()
        assert wifi_write[2]["ApBroadcastDisabled"] == "1" and wifi_write[2]["ApIsolate"] == "1"
        assert wifi_write[2]["wifi_syncparas_flag"] == "0"
        row = control(port, "wifi.status", {})[1]["result"]["main_2g"]
        assert row["ssid"] == "renamed-24" and row["key"] == "new-password-8"
        assert row["hidden"] == "1" and row["isolate"] == "1"
        # Switch-only save mirrors the WebUI: nothing but the switch status.
        mark = len(writes())
        status, result = control(port, "wifi.configure", {"section": "main_2g", "enabled": False})
        assert status == 200 and result["result"]["changed"] is True, result
        switch_write = next(w for w in writes()[mark:] if w[1] == "setAccessPointInfo")
        assert switch_write[2]["AccessPointSwitchStatus"] == "0" and "SSID" not in switch_write[2]
        assert control(port, "wifi.configure", {"section": "main_2g", "enabled": True})[0] == 200
        # Empty key keeps the current password; invalid values are refused.
        status, result = control(port, "wifi.configure", {"section": "main_2g", "ssid": "renamed-24", "key": ""})
        assert status == 200 and result["result"]["changed"] is False, result
        assert control(port, "wifi.configure", {"section": "main_2g", "key": "short"})[0] == 400
        assert control(port, "wifi.configure", {"section": "main_2g", "ssid": "x" * 33})[0] == 400
        assert control(port, "wifi.configure", {"section": "wifi_6g", "ssid": "nope"})[0] == 400
        # 2.4G accepts 1-13 (and 0=auto); 5G-only channels refuse on 2.4G.
        assert control(port, "wifi.configure", {"section": "main_2g", "channel": "36"})[0] == 400
        assert control(port, "wifi.configure", {"section": "main_2g", "channel": "6"})[0] == 200

        # 5G channel pinning: the per-radio write re-sends the current
        # advanced set with only the channel moved.
        mark = len(writes())
        status, result = control(port, "wifi.configure", {"section": "main_5g", "channel": "149"})
        assert status == 200 and result["result"]["changed"] is True, result
        adv_write = next(w for w in writes()[mark:] if w[1] == "setWiFiChipAdvancedInfo")
        assert adv_write[2] == {"goformId": "setWiFiChipAdvancedInfo", "ChipIndex": "1",
                                "WirelessMode": "6", "CountryCode": "CN", "Channel": "149",
                                "BandWidth": "6", "Band": "a"}, adv_write
        assert CHIP_ADVANCED["ResponseList"][1]["Channel"] == "149"
        status, result = control(port, "wifi.configure", {"section": "main_5g", "channel": "149"})
        assert status == 200 and result["result"]["changed"] is False, result
        assert control(port, "wifi.configure", {"section": "main_5g", "channel": "0"})[0] == 200
        assert CHIP_ADVANCED["ResponseList"][1]["Channel"] == "0"
        assert control(port, "wifi.configure", {"section": "main_5g", "channel": "50"})[0] == 400

        # Band steering toggle rides the module-switch goform with the
        # current switch and LAN flags preserved.
        mark = len(writes())
        status, result = control(port, "wifi.set_dual_band", {"enabled": True})
        assert status == 200 and result["result"] == {"enabled": True, "changed": True, "verified": True}, result
        steering = next(w for w in writes()[mark:] if w[1] == "switchWiFiModule")
        assert steering[2]["wifi_lbd_enable"] == "1" and steering[2]["SwitchOption"] == "1"
        assert control(port, "wifi.dual_band_status", {})[1]["result"]["enabled"] is True
        status, result = control(port, "wifi.set_dual_band", {"enabled": True})
        assert status == 200 and result["result"]["changed"] is False, result
        assert control(port, "wifi.set_dual_band", {"enabled": 2})[0] == 400
        assert control(port, "wifi.set_dual_band", {})[0] == 400
        control(port, "wifi.set_dual_band", {"enabled": False})

        # Neighbor monitor: disabled by default, mainline-shaped /state object
        # and session-scoped enable through the OEM lists.
        state = call(port, "/state")[1]
        assert state["neighbor"]["status"] == "disabled" and state["neighbor"]["cells"] == [], state["neighbor"]
        status, result = control(port, "neighbor.status", {})
        assert status == 200 and result["result"]["enabled"] is False, result
        assert control(port, "neighbor.set", {"enabled": "yes"})[0] == 400
        status, result = control(port, "neighbor.set", {"enabled": True})
        assert status == 200 and result["result"]["status"] == "ready", result
        assert result["result"]["source"] == "oem_goform"
        # /state serves the monitor live: an immediate read (before the next
        # sampler tick) must already reflect the toggle.
        state = call(port, "/state")[1]["neighbor"]
        assert state["enabled"] is True and state["collector_running"] is True, state
        assert state["sampled_at"] and state["age_ms"] >= 0
        lte, nr = state["cells"][0], next(c for c in state["cells"] if c["rat"] == "NR5G")
        assert (lte["rat"], lte["pci"], lte["arfcn"], lte["band"], lte["rsrp_dbm"]) == ("LTE", 57, 3725, 3, -96)
        assert (nr["rat"], nr["band"]) == ("NR5G", 78)
        assert control(port, "neighbor.set", {"enabled": False})[1]["result"]["status"] == "disabled"
        assert call(port, "/state")[1]["neighbor"]["enabled"] is False

        # Traffic: the whole OEM limit block is re-sent, untouched fields preserved.
        status, result = control(port, "traffic.set_limit", {"enabled": 1, "type": 1, "value": "107374182400", "ratio": 90})
        assert status == 200 and result["result"]["verified"], result
        assert writes()[-1][2] == {
            "goformId": "DATA_LIMIT_SETTING", "wan_auto_clear_flow_data_switch": "on", "traffic_clear_date": "1",
            "flux_limited_disconnect": "off", "data_volume_limit_switch": "1", "notify_deviceui_enable": "0",
            "data_volume_limit_unit": "data", "data_volume_limit_size": "107374182400", "data_volume_alert_percent": "90"}
        status, result = control(port, "traffic.set_clear_day", {"day": 15, "enabled": 0})
        assert status == 200 and result["result"]["verified"], result
        assert writes()[-1][2]["traffic_clear_date"] == "15" and writes()[-1][2]["wan_auto_clear_flow_data_switch"] == "off"
        assert writes()[-1][2]["data_volume_limit_size"] == "107374182400", "limit fields must be preserved"
        status, result = control(port, "traffic.set_limit", {"enabled": 0})
        assert status == 200 and "data_volume_limit_size" not in writes()[-1][2] and writes()[-1][2]["data_volume_limit_switch"] == "0"
        for bad in ({"enabled": 2}, {"enabled": 1, "type": 3, "value": "1"}, {"enabled": 1, "type": 1, "value": "0"},
                    {"enabled": 1, "type": 1, "value": "1;x"}, {"enabled": 1, "type": 1, "value": "1", "ratio": 101}):
            assert control(port, "traffic.set_limit", bad)[0] == 400, bad
        assert control(port, "traffic.set_clear_day", {"day": 32})[0] == 400
        _, traffic_state = call(port, "/state")
        # State comes from the firmware cfg store (a static mock here), in the mainline shape.
        assert traffic_state["traffic"]["clear_day"] == {"clearday": 1, "enable": 1}, traffic_state["traffic"]
        assert traffic_state["traffic"]["limit"]["enable"] == 0

        # Clients: lists come from the firmware, access control edits the OEM blacklist.
        _, client_state = call(port, "/state")
        clients = client_state["clients"]
        assert clients["wifi"] == 1 and clients["lan"] == 1 and clients["total"] == 2, clients
        assert {c["mac"] for c in clients["list"]} == {"aa:bb:cc:00:00:01", "aa:bb:cc:00:00:02"}
        status, result = control(port, "client.block", {"mac": "AA:BB:CC:00:00:01"})
        assert status == 200 and result["result"]["verified"], result
        assert writes()[-1][2] == {"goformId": "setDeviceAccessControlList", "AclMode": "2", "WhiteMacList": "",
                                   "BlackMacList": "aa:bb:cc:00:00:01;", "WhiteNameList": "", "BlackNameList": "phone;"}
        for _ in range(40):  # /state is refreshed by the collector loop
            if call(port, "/state")[1]["clients"].get("blocked") == ["aa:bb:cc:00:00:01"]:
                break
            time.sleep(0.5)
        else:
            raise AssertionError(call(port, "/state")[1]["clients"])
        assert control(port, "client.unblock", {"mac": "aa:bb:cc:00:00:01"})[0] == 200
        assert writes()[-1][2]["BlackMacList"] == "" and STORE["BlackMacList"] == ""
        status, result = control(port, "client.kick", {"macs": "aa:bb:cc:00:00:02"})
        assert status == 200 and result["result"]["kicked"] == ["aa:bb:cc:00:00:02"], result
        assert STORE["BlackMacList"] == "", "kick must leave the client allowed"
        assert control(port, "client.rename", {"mac": "aa:bb:cc:00:00:01", "hostname": "living-room"})[0] == 200
        assert writes()[-1][2] == {"goformId": "EDIT_HOSTNAME", "mac": "aa:bb:cc:00:00:01", "hostname": "living-room"}
        for action, bad in (("client.block", {"mac": "nope"}), ("client.kick", {"macs": ""}),
                            ("client.rename", {"mac": "aa:bb:cc:00:00:01", "hostname": "a;b"})):
            assert control(port, action, bad)[0] == 400, (action, bad)

        # LAN / MTU / Wi-Fi switch.
        status, result = control(port, "lan.set", {"dhcp_start": "192.168.0.50", "dhcp_end": "192.168.0.200", "lease_seconds": 7200})
        assert status == 200 and result["result"]["verified"], result
        assert writes()[-1][2] == {"goformId": "DHCP_SETTING", "lanIp": "192.168.0.1", "lanNetmask": "255.255.255.0",
                                   "lanDhcpType": "SERVER", "dhcpStart": "192.168.0.50", "dhcpEnd": "192.168.0.200",
                                   "dhcpLease": "2", "dhcp_reboot_flag": "1", "mac_ip_reset": "0"}
        status, result = control(port, "lan.set", {"dhcp_disabled": 1})
        assert status == 200 and STORE["dhcpEnabled"] == "0" and "dhcpStart" not in writes()[-1][2]
        assert control(port, "lan.set", {"dhcp_disabled": 0})[0] == 200
        for bad in ({"ip": "10.0.0.1"}, {"dhcp_start": "10.0.0.5"}, {"dhcp_start": "192.168.0.250", "dhcp_end": "192.168.0.10"},
                    {"dhcp_start": "192.168.0.1"}, {"lease_seconds": 100}, {"dhcp_disabled": 2}):
            assert control(port, "lan.set", bad)[0] == 400, bad
        status, result = control(port, "lan.set_mtu", {"mtu": 1400})
        assert status == 200 and result["result"]["verified"] and writes()[-1][2] == {"goformId": "SET_DEVICE_MTU", "mtu": "1400", "tcp_mss": "1360"}
        assert control(port, "lan.set_mtu", {"mtu": 100})[0] == 400
        status, result = control(port, "wifi.set_module", {"enabled": 0})
        assert status == 200 and result["result"]["verified"] and writes()[-1][2] == {"goformId": "switchWiFiModule", "SwitchOption": "0"}
        assert control(port, "wifi.set_module", {"enabled": 1})[0] == 200
        assert control(port, "wifi.set_module", {"enabled": 5})[0] == 400

        # Device actions and unmapped actions.
        assert control(port, "device.reboot", {})[0] == 200 and writes()[-1][1] == "REBOOT_DEVICE"
        assert control(port, "wifi.configure", {"section": "main_2g", "bogus": "x"})[0] == 400
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
