#!/usr/bin/env python3
"""Offline HTTP and firmware-cfg contract test for the U50 candidate."""
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
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

BINARY = sys.argv[1] if len(sys.argv) > 1 else "rust/target/debug/zwrt-datad"

class Handler(BaseHTTPRequestHandler):
    reply = b'{"model_name":"U50Pro","wa_inner_version":"B02","network_type":"NR5G","network_provider_fullname":"Test","battery_value":"84","sim_iccid":"private"}'
    query = None
    host = None
    referer = None
    def do_GET(self):
        path = urlparse(self.path)
        Handler.query = (path.path, parse_qs(path.query))
        Handler.host = self.headers.get("Host")
        Handler.referer = self.headers.get("Referer")
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        if Handler.host == "192.168.0.1":
            self.wfile.write(Handler.reply)
        else:
            self.wfile.write(b'{"network_type":""}')
    def log_message(self, *_args):
        pass

server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
thread = threading.Thread(target=server.serve_forever, daemon=True)
thread.start()
url = f"http://127.0.0.1:{server.server_port}/goform/goform_get_cmd_process"
try:
    with tempfile.TemporaryDirectory(prefix="u50-cfg-test-") as tmp:
        cfg = Path(tmp) / "cfg"
        cfg.write_text('''#!/bin/sh
case "$1:$2" in
  get:model_name) printf '%s\\n' "${MOCK_U50_MODEL:-U50Pro}" ;;
  get:modem_msn) if [ "${MOCK_U50_NO_ID:-0}" = 1 ]; then exit 1; fi; printf '%s\\n' 'MSN-fixture' ;;
  get:integrate_version) printf '%s\\n' 'B02' ;;
  get:lan_ipaddr) printf '%s\\n' '192.168.0.1' ;;
  get:lan_netmask) printf '%s\\n' '255.255.255.0' ;;
  get:wan_ipaddr) printf '%s\\n' '10.0.0.2' ;;
  get:wan_gateway) printf '%s\\n' '10.0.0.1' ;;
  get:ppp_status) printf '%s\\n' 'connected' ;;
  get:network_type) printf '%s\\n' 'LTE' ;;
  get:network_provider_fullname) printf '%s\\n' 'TestCfg' ;;
  get:signalbar) printf '%s\\n' '5' ;;
  get:battery_vol_percent) printf '%s\\n' '66' ;;
  get:battery_temp) printf '%s\\n' '34' ;;
  get:realtime_tx_thrpt) printf '%s\\n' '4980' ;;
  get:realtime_rx_thrpt) printf '%s\\n' '1814' ;;
  get:wifi_onoff_state) printf '%s\\n' '1' ;;
  get:wifi_access_sta_num) printf '%s\\n' '2' ;;
  get:lte_rsrp) printf '%s\\n' '-83' ;;
  get:simcard_active_slot) printf '%s\\n' '1' ;;
  *) exit 1 ;;
esac
''')
        cfg.chmod(0o700)
        # Fixture tree for the kernel-level readers (thermal/battery/net/proc).
        root = Path(tmp) / "root"
        for name, values in {
            "sys/class/thermal/thermal_zone0": {"type": "cpu0-0-usr", "temp": "42800"},
            "sys/class/thermal/thermal_zone1": {"type": "modem-mmw0-usr", "temp": "-273000"},
            "sys/class/power_supply/battery_zte": {"online": "1", "status": "Charging", "health": "Good"},
            "sys/class/power_supply/battery": {"voltage_now": "4360000", "current_now": "-114000", "cycle_count": "103"},
            "sys/class/power_supply/charger_zte": {"present": "1", "type": "Mains"},
            "sys/class/net/bridge0": {"flags": "0x1103", "address": "b8:d4:bc:00:00:01"},
            "sys/class/net/rmnet_data0": {"flags": "0x1"},
            "sys/class/net/wlan0": {"address": "b8:d4:bc:00:00:02"},
            "proc/sys/kernel": {"hostname": "sdxprairie", "osrelease": "4.14.206"},
        }.items():
            (root / name).mkdir(parents=True)
            for file, text in values.items():
                (root / name / file).write_text(text + "\n")
        (root / "proc/uptime").write_text("100.5 90.0\n")
        (root / "proc/meminfo").write_text("MemTotal: 1000 kB\nMemAvailable: 250 kB\n")
        (root / "proc/net").mkdir(parents=True, exist_ok=True)
        (root / "proc/net/arp").write_text(
            "IP address       HW type     Flags       HW address            Mask     Device\n"
            "192.168.0.7      0x1         0x2         aa:bb:cc:00:11:22     *        bridge0\n")
        (root / "etc_rw/ztembb/configs").mkdir(parents=True)
        (root / "etc_rw/ztembb/configs/dnsmasq.leases").write_text("1790785134 aa:bb:cc:00:11:22 192.168.0.7 PHONE 01\n")
        ip = Path(tmp) / "ip"
        ip.write_text('''#!/bin/sh
case "$*" in
  *"-4 addr show dev bridge0"*) echo '11: bridge0    inet 192.168.0.1/24 brd 192.168.0.255 scope global bridge0\\ valid_lft forever' ;;
  *"-4 addr show dev rmnet_data0"*) echo '12: rmnet_data0    inet 10.38.1.22/30 scope global rmnet_data0\\ valid_lft forever' ;;
  *"-6 addr show dev rmnet_data0"*) echo '12: rmnet_data0    inet6 2001:db8::5/64 scope global \\ valid_lft forever' ;;
esac
''')
        ip.chmod(0o700)
        iw = Path(tmp) / "iw"
        iw.write_text('''#!/bin/sh
printf 'Station b8:d4:bc:00:00:01 (on wlan0)\\nStation aa:bb:cc:00:11:22 (on wlan0)\\n'
''')
        iw.chmod(0o700)
        env = {**os.environ, "ZWRT_DATAD_U50_CFG_BIN": str(cfg), "ZWRT_DATAD_U50_ROOT": str(root),
               "ZWRT_DATAD_U50_IP_BIN": str(ip), "ZWRT_DATAD_U50_IW_BIN": str(iw)}
        cmd = [BINARY, "--u50-model", "u50pro", "--u50-loopback-only", "--u50-goform-url", url, "--once"]
        result = subprocess.run(cmd, env=env, capture_output=True, text=True, timeout=15, check=True)
        state = json.loads(result.stdout)
        assert state["device"]["api_template"] == "U50PRO"
        assert state["device"]["api_template_supported"] == 0
        assert state["system"]["sw_version"] == "B02"
        assert state["dhcp"]["ip"] == "192.168.0.1"
        assert state["u50_cfg"]["wan_ipaddr"] == "10.0.0.2"
        assert state["net"]["type"] == "NR5G"
        assert state["net"]["bars"] == 5
        assert state["battery"]["percent"] == 66
        assert state["traffic"]["tx_speed"] == 4980
        assert state["wlan"]["enabled"] == 1
        # Live station list (1 real client) wins over the OEM counter (2).
        assert state["clients"]["wifi"] == 1
        assert state["sim"]["current_slot"] == 1
        assert "sim_iccid" not in result.stdout
        assert state["thermal"]["cpu_celsius"] == 43
        assert [z["name"] for z in state["thermal"]["zones"]] == ["cpu0-0-usr"]
        assert state["system"]["mem_used_pct"] == 75 and state["system"]["uptime"] == 100
        assert state["system"]["hostname"] == "sdxprairie"
        assert state["battery"]["charging"] == 1 and state["battery"]["bat_ua"] == 114000
        assert state["battery"]["charger_type_name"] == "Mains"
        assert state["interfaces"]["wan4"]["ipv4"] == [{"address": "10.38.1.22", "mask": 30}]
        assert state["interfaces"]["wan6"]["ipv6"] == [{"address": "2001:db8::5", "mask": 64}]
        assert state["interfaces"]["lan"]["up"] is True
        # The AP's own address is skipped; the lease supplies the name.
        assert state["clients"]["wifi"] == 1 and state["clients"]["list"] == [
            {"name": "PHONE", "ip": "192.168.0.7", "mac": "aa:bb:cc:00:11:22"}]
        # CPU/memory/connections come from the shared sampler on the real /proc.
        assert {"cpu_usage_tenths", "memory_kb", "storage", "throughput"} <= set(state["runtime"])
        assert Handler.query[0] == "/goform/goform_get_cmd_process"
        assert Handler.host == "192.168.0.1"
        assert Handler.referer == "http://192.168.0.1/"
        assert "model_name" in Handler.query[1]["cmd"][0]
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", 0))
            port = probe.getsockname()[1]
        ota_dir = Path(tmp) / "otadata"
        ota_dir.mkdir()
        server_env = {**env, "ZWRT_DATAD_OTA_DISABLE_AUTO": "1"}
        process = subprocess.Popen([BINARY, "--u50-model", "u50pro", "--u50-loopback-only", "--u50-goform-url", url, "--port", str(port),
                                    "--u50-data-dir", str(ota_dir)], env=server_env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
        try:
            for _ in range(40):
                try:
                    with urllib.request.urlopen(f"http://127.0.0.1:{port}/state", timeout=1) as response:
                        assert json.load(response)["device"]["api_template_supported"] == 0
                    break
                except urllib.error.URLError:
                    time.sleep(0.1)
            else:
                raise AssertionError("candidate server did not start")
            with urllib.request.urlopen(f"http://127.0.0.1:{port}/capabilities", timeout=2) as response:
                capabilities = json.load(response)
                # The U50S offers the mainline control API for what it implements.
                assert "cellular.set" in capabilities["controls"] and "sms.send_raw" in capabilities["controls"]
                assert not any(name.startswith("speedtest") for name in capabilities["controls"])
                assert capabilities["events"] == ["state"]
            with urllib.request.urlopen(f"http://127.0.0.1:{port}/events", timeout=2) as response:
                event_lines = [response.readline().decode().strip() for _ in range(3)]
                payload = next(line.removeprefix("data: ") for line in event_lines if line.startswith("data: "))
                assert json.loads(payload)["device"]["api_template"] == "U50PRO"
            # Mainline-style local OTA API: config, status and rejection of bad input.
            with urllib.request.urlopen(f"http://127.0.0.1:{port}/ota/status", timeout=2) as response:
                status = json.load(response)
                assert status["status"]["state"] == "idle" and status["enabled"] is True
            with urllib.request.urlopen(f"http://127.0.0.1:{port}/ota/config", timeout=2) as response:
                assert json.load(response)["config"]["sources"] == ["custom", "netdisk", "github"]
            bad = urllib.request.Request(f"http://127.0.0.1:{port}/ota/config", data=b'{"servers":["http://192.168.0.9/x"]}',
                                         headers={"Content-Type": "application/json"}, method="POST")
            try:
                urllib.request.urlopen(bad, timeout=2)
                raise AssertionError("plain-http server accepted")
            except urllib.error.HTTPError as error:
                assert error.code == 400, error.code
            try:
                urllib.request.urlopen(f"http://127.0.0.1:{port}/control", timeout=2)
                raise AssertionError("control accepted a GET")
            except urllib.error.HTTPError as error:
                assert error.code == 405, error.code
        finally:
            process.terminate()
            process.wait(timeout=5)
        # Existing service definitions pass --u50-panel-config; the directory of
        # that file is then the data directory (same cloud.json as before).
        legacy_dir = Path(tmp) / "legacy"
        legacy_dir.mkdir()
        legacy_cmd = [BINARY, "--u50-model", "u50pro", "--u50-loopback-only", "--u50-goform-url", url, "--u50-enable-writes",
                      "--u50-panel-config", str(legacy_dir / "cloud.json"), "--port", str(port)]
        legacy = subprocess.Popen(legacy_cmd, env=server_env, stdout=subprocess.DEVNULL,
                                  stderr=subprocess.PIPE, text=True)
        try:
            for _ in range(40):
                try:
                    with urllib.request.urlopen(f"http://127.0.0.1:{port}/state", timeout=1) as response:
                        assert json.load(response)["device"]["api_template"] == "U50PRO"
                    break
                except urllib.error.URLError:
                    time.sleep(0.1)
            else:
                raise AssertionError("legacy service arguments no longer start the server")
            save = urllib.request.Request(f"http://127.0.0.1:{port}/ota/config",
                                          data=b'{"enabled":true,"servers":[],"sources":["github"]}',
                                          headers={"Content-Type": "application/json"}, method="POST")
            with urllib.request.urlopen(save, timeout=2) as response:
                assert json.load(response)["success"] is True
            assert (legacy_dir / "ota.json").exists(), "state must live next to cloud.json"
        finally:
            legacy.terminate()
            legacy.wait(timeout=5)
        Handler.reply = b"<html>login required</html>"
        fallback = subprocess.run(cmd, env=env, capture_output=True, text=True, timeout=15, check=True)
        partial = json.loads(fallback.stdout)
        assert partial["u50_sources"]["goform"] == "unavailable"
        assert partial["device"]["model_name"] == "U50Pro"
        assert partial["net"]["type"] == "LTE"
        assert partial["net"]["operator"] == "TestCfg"
        u50s_env = {**env, "MOCK_U50_MODEL": "U50S"}
        u50s = subprocess.run([BINARY, "--u50-model", "u50s", "--u50-loopback-only", "--u50-goform-url", url, "--once"], env=u50s_env, capture_output=True, text=True, timeout=15, check=True)
        assert json.loads(u50s.stdout)["device"]["api_template"] == "U50S"
        enroll_dir = Path(tmp) / "enroll"
        enrollment = json.dumps({"username": "enr_" + "a" * 32, "password": "fixture-secret"})
        enroll_cmd = [BINARY, "--u50-model", "u50s", "--u50-loopback-only", "--u50-enroll-dir", str(enroll_dir)]
        enrolled = subprocess.run(enroll_cmd, input=enrollment, env=u50s_env,
                                  capture_output=True, text=True, timeout=15, check=True)
        assert json.loads(enrolled.stdout) == {"configured": True, "model": "U50S", "password_configured": True}
        assert "fixture-secret" not in enrolled.stdout and "MSN-fixture" not in enrolled.stdout
        cloud_file = enroll_dir / "cloud.json"
        saved = json.loads(cloud_file.read_text())
        assert cloud_file.stat().st_mode & 0o077 == 0
        assert saved["username"] == "enr_" + "a" * 32
        assert saved["password"] == "fixture-secret"
        assert saved["identity"] == "7d77fceb-0592-529a-8d12-2b54068aaaff"
        assert saved["remote_panel_control_enabled"] is False
        assert saved["remote_webshell_enabled"] is False
        assert "MSN-fixture" not in cloud_file.read_text()
        saved_bytes = cloud_file.read_bytes()
        duplicate = subprocess.run(enroll_cmd, input=enrollment, env=u50s_env,
                                   capture_output=True, text=True, timeout=15)
        assert duplicate.returncode != 0 and cloud_file.read_bytes() == saved_bytes
        no_identity = subprocess.run([BINARY, "--u50-model", "u50s", "--u50-loopback-only", "--u50-enroll-dir", str(Path(tmp) / "no-id")],
                                     input=enrollment, env={**u50s_env, "MOCK_U50_NO_ID": "1"},
                                     capture_output=True, text=True, timeout=15)
        assert no_identity.returncode != 0
        assert "fixture-secret" not in no_identity.stderr
        wrong = subprocess.run([BINARY, "--u50-model", "u50s", "--u50-loopback-only", "--u50-goform-url", url, "--once"], env=env, capture_output=True, text=True, timeout=15)
        assert wrong.returncode != 0
        print("U50 firmware cfg and optional GoAhead probe OK")
finally:
    server.shutdown()
    server.server_close()
