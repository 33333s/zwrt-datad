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
        env = {**os.environ, "ZWRT_DATAD_U50_CFG_BIN": str(cfg)}
        cmd = [BINARY, "--u50-model", "u50pro", "--u50-goform-url", url, "--once"]
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
        assert state["clients"]["wifi"] == 2
        assert state["sim"]["current_slot"] == 1
        assert "sim_iccid" not in result.stdout
        assert Handler.query[0] == "/goform/goform_get_cmd_process"
        assert Handler.host == "192.168.0.1"
        assert Handler.referer == "http://192.168.0.1/"
        assert "model_name" in Handler.query[1]["cmd"][0]
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", 0))
            port = probe.getsockname()[1]
        process = subprocess.Popen([BINARY, "--u50-model", "u50pro", "--u50-goform-url", url, "--port", str(port)], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
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
                assert capabilities["controls"] == []
                assert capabilities["events"] == ["state"]
            with urllib.request.urlopen(f"http://127.0.0.1:{port}/events", timeout=2) as response:
                event_lines = [response.readline().decode().strip() for _ in range(3)]
                payload = next(line.removeprefix("data: ") for line in event_lines if line.startswith("data: "))
                assert json.loads(payload)["device"]["api_template"] == "U50PRO"
            try:
                urllib.request.urlopen(f"http://127.0.0.1:{port}/control", timeout=2)
                raise AssertionError("control route was exposed")
            except urllib.error.HTTPError as error:
                assert error.code == 404
        finally:
            process.terminate()
            process.wait(timeout=5)
        Handler.reply = b"<html>login required</html>"
        fallback = subprocess.run(cmd, env=env, capture_output=True, text=True, timeout=15, check=True)
        partial = json.loads(fallback.stdout)
        assert partial["u50_sources"]["goform"] == "unavailable"
        assert partial["device"]["model_name"] == "U50Pro"
        assert partial["net"]["type"] == "LTE"
        assert partial["net"]["operator"] == "TestCfg"
        u50s_env = {**env, "MOCK_U50_MODEL": "U50S"}
        u50s = subprocess.run([BINARY, "--u50-model", "u50s", "--u50-goform-url", url, "--once"], env=u50s_env, capture_output=True, text=True, timeout=15, check=True)
        assert json.loads(u50s.stdout)["device"]["api_template"] == "U50S"
        wrong = subprocess.run([BINARY, "--u50-model", "u50s", "--u50-goform-url", url, "--once"], env=env, capture_output=True, text=True, timeout=15)
        assert wrong.returncode != 0
        print("U50 firmware cfg and optional GoAhead probe OK")
finally:
    server.shutdown()
    server.server_close()
