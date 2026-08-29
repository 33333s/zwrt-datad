#!/bin/sh
set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${1:-$ROOT/zwrt-datad-test}"
PORT="${ZWRT_DATAD_TEST_PORT:-19460}"
LAN_LOCAL_PORT=$((PORT + 1))
LAN_PORT=$((PORT + 2))
TOPFLOW_PORT=$((PORT + 3))
MC7523_PORT=$((PORT + 4))
MC8532B_PORT=$((PORT + 5))
TOKEN_FILE="$ROOT/tests/auth.token"
CALL_LOG="$ROOT/tests/mock-calls.log"
CHILD_FD_LOG="$ROOT/tests/child-fds.log"
MU5252_CALL_LOG="$ROOT/tests/mu5252-calls.log"
INVALID_JSON_OUT="$ROOT/tests/invalid-json.out"
INVALID_PARAM_OUT="$ROOT/tests/invalid-param.out"
INVALID_WIFI_OUT="$ROOT/tests/invalid-wifi.out"
THERMAL_FIXTURE="$ROOT/tests/fixtures/thermal"
COOLING_FIXTURE="$ROOT/tests/fixtures/cooling"
COOLING_TMP="$(mktemp -d)"
cp -R "$COOLING_FIXTURE/." "$COOLING_TMP/"
PID=""
LAN_PID=""
TOPFLOW_PID=""
MC7523_PID=""
MC8532B_PID=""

sh -n "$ROOT/scripts/service.sh"
grep -F 'SERVICE_DIR="${ZWRT_DATAD_DIR:-/data/zwrt-datad}"' "$ROOT/scripts/service.sh" >/dev/null
grep -F 'PID_FILE="${ZWRT_DATAD_PID_FILE:-$SERVICE_DIR/zwrt-datad.pid}"' "$ROOT/scripts/service.sh" >/dev/null
if grep -F '/etc/init.d' "$ROOT/scripts/service.sh" >/dev/null; then
    echo 'service.sh must not use /etc/init.d' >&2
    exit 1
fi
grep -F 'statvfs("/data", &disk)' "$ROOT/src/system_ext.c" >/dev/null

cleanup() {
    [ -n "$PID" ] && kill "$PID" 2>/dev/null || true
    [ -n "$PID" ] && wait "$PID" 2>/dev/null || true
    [ -n "$LAN_PID" ] && kill "$LAN_PID" 2>/dev/null || true
    [ -n "$LAN_PID" ] && wait "$LAN_PID" 2>/dev/null || true
    [ -n "$TOPFLOW_PID" ] && kill "$TOPFLOW_PID" 2>/dev/null || true
    [ -n "$TOPFLOW_PID" ] && wait "$TOPFLOW_PID" 2>/dev/null || true
    [ -n "$MC7523_PID" ] && kill "$MC7523_PID" 2>/dev/null || true
    [ -n "$MC7523_PID" ] && wait "$MC7523_PID" 2>/dev/null || true
    [ -n "$MC8532B_PID" ] && kill "$MC8532B_PID" 2>/dev/null || true
    [ -n "$MC8532B_PID" ] && wait "$MC8532B_PID" 2>/dev/null || true
    rm -f "$TOKEN_FILE" "$CALL_LOG" "$CHILD_FD_LOG" "$MU5252_CALL_LOG" \
        "$INVALID_JSON_OUT" "$INVALID_PARAM_OUT" "$INVALID_WIFI_OUT"
    rm -rf "$COOLING_TMP"
}
trap cleanup EXIT INT TERM

printf '%s\n' 'fixture-private-token' >"$TOKEN_FILE"
: >"$CALL_LOG"
: >"$CHILD_FD_LOG"
chmod +x "$ROOT/tests/mock_ubus.sh" "$ROOT/tests/mock_uci.sh" "$ROOT/tests/mock_adb.sh"
export ZWRT_DATAD_THERMAL_ROOT="$THERMAL_FIXTURE"

ZWRT_DATAD_UBUS_BIN="$ROOT/tests/mock_ubus.sh" \
ZWRT_DATAD_UCI_BIN="$ROOT/tests/mock_uci.sh" \
MOCK_CALL_LOG="$CALL_LOG" \
ZWRT_DATAD_FD_DUMP="$CHILD_FD_LOG" \
"$BIN" -i 200 -p "$PORT" --auth-token-file "$TOKEN_FILE" >/dev/null 2>&1 &
PID=$!

i=0
until curl -fsS "http://127.0.0.1:$PORT/healthz" >/dev/null; do
    i=$((i + 1))
    [ "$i" -lt 50 ] || { echo 'server did not start' >&2; exit 1; }
    sleep 0.1
done

code="$(curl -sS -o /dev/null -w '%{http_code}' "http://127.0.0.1:$PORT/state")"
[ "$code" = "401" ]

# issue #26: external commands (here the ubus mock) must not inherit datad's
# listener/client sockets, otherwise a persistent grandchild keeps the port in
# LISTEN after datad exits. Linux-only (needs /proc).
if [ -d /proc/1/fd ]; then
    sleep 0.5
    if [ -s "$CHILD_FD_LOG" ]; then
        echo 'exec-ed child inherited datad sockets:' >&2
        cat "$CHILD_FD_LOG" >&2
        exit 1
    fi
fi

curl -fsS -H 'Authorization: Bearer fixture-private-token' \
    "http://127.0.0.1:$PORT/state" | python3 -c '
import json, sys
data = json.load(sys.stdin)
assert data["device"]["api_template"] == "MU5250"
assert data["device"]["api_template_supported"] == 1
assert data["device"]["full_ubus"] == 1
assert data["modems"] == []
assert data["net"]["lte_supported_bands"] == "1,3"
assert data["net"]["nr_sa_supported_bands"] == "78"
assert data["net"]["nr_nsa_supported_bands"] == ""
assert data["battery"]["percent"] == 0
assert data["nfc"]["switch"] == 0
assert data["thermal"]["cpu_celsius"] == 42
assert data["thermal"]["zones"] == [
    {"name": "battery", "celsius": 30.0},
    {"name": "cpuss-0", "celsius": 41.25},
]
'

# SSE stream must stay spec-compliant even though traffic.limit is a verbatim
# passthrough of pretty-printed (multi-line) ubus JSON. Every payload line has
# to carry its own "data:" prefix so a compliant client can reassemble the
# whole snapshot instead of just the truncated first line (issue #17).
python3 - "$PORT" <<'PY'
import json, socket, sys, time

req = (
    b"GET /events HTTP/1.1\r\n"
    b"Host: 127.0.0.1\r\n"
    b"Authorization: Bearer fixture-private-token\r\n"
    b"Accept: text/event-stream\r\n\r\n"
)
buf = b""
with socket.create_connection(("127.0.0.1", int(sys.argv[1])), timeout=5) as sock:
    sock.sendall(req)
    sock.settimeout(5)
    deadline = time.time() + 5
    while time.time() < deadline:
        chunk = sock.recv(4096)
        if not chunk:
            break
        buf += chunk
        idx = buf.find(b"event: state")
        if idx != -1 and b"\n\n" in buf[idx:]:
            break

head, _, body = buf.partition(b"\r\n\r\n")
assert head.startswith(b"HTTP/1.1 200"), head
assert b"text/event-stream" in head, head

start = body.find(b"event: state")
assert start != -1, body[:200]
event = body[start:].split(b"\n\n", 1)[0]

data_lines = []
for line in event.split(b"\n"):
    assert line.startswith(b"event:") or line.startswith(b"data:"), line
    if line.startswith(b"data:"):
        data_lines.append(line[5:].lstrip(b" "))

snap = json.loads(b"\n".join(data_lines))
assert snap["traffic"]["limit"]["value"] == "1610612736000", snap["traffic"]["limit"]
assert "system" in snap, sorted(snap)
PY

MOCK_MODEL_NAME=CPE_FIXTURE \
MOCK_NO_BATTERY=1 \
MOCK_NO_NFC=1 \
ZWRT_DATAD_UBUS_BIN="$ROOT/tests/mock_ubus.sh" \
ZWRT_DATAD_UCI_BIN="$ROOT/tests/mock_uci.sh" \
"$BIN" --once | python3 -c '
import json, sys
data = json.load(sys.stdin)
assert data["device"]["api_template"] == "legacy_compat"
assert data["device"]["api_template_supported"] == 0
assert "battery" not in data
assert "nfc" not in data
'

curl -fsS -H 'X-Auth-Token: fixture-private-token' \
    "http://127.0.0.1:$PORT/capabilities" | python3 -m json.tool >/dev/null

curl -fsS -H 'X-Auth-Token: fixture-private-token' \
    "http://127.0.0.1:$PORT/ubus" | grep -F 'zte_nwinfo_api' >/dev/null
curl -fsS -H 'X-Auth-Token: fixture-private-token' \
    "http://127.0.0.1:$PORT/ubus?verbose=1" | grep -F 'nwinfo_get_msim_netinfo' >/dev/null

curl -fsS \
    -H 'X-Auth-Token: fixture-private-token' \
    -H 'Content-Type: application/json' \
    -d '{"service":"system","method":"info","args":{}}' \
    "http://127.0.0.1:$PORT/ubus/call" | python3 -c '
import json, sys
data = json.load(sys.stdin)
assert data["ok"] is True
assert data["result"]["uptime"] == 123
'

curl -fsS -H 'Authorization: Bearer fixture-private-token' \
    -H 'Content-Type: application/json' \
    -d '{"action":"network.set_mode","params":{"mode":"Only_LTE"}}' \
    "http://127.0.0.1:$PORT/control" | python3 -c \
    'import json,sys; data=json.load(sys.stdin); assert data["ok"] is True'

grep -F 'zte_nwinfo_api' "$CALL_LOG" | grep -F 'nwinfo_set_netselect' | grep -F 'Only_LTE' >/dev/null

curl -fsS -H 'Authorization: Bearer fixture-private-token' \
    -H 'Content-Type: application/json' \
    -d '{"action":"cellular.set","params":{"roaming":1}}' \
    "http://127.0.0.1:$PORT/control" | python3 -c \
    'import json,sys; data=json.load(sys.stdin); assert data["ok"] is True'

wwan_call="$(grep -F 'zwrt_data' "$CALL_LOG" | grep -F 'set_wwaniface' | tail -n 1)"
printf '%s\n' "$wwan_call" | grep -F '"roam_enable":1' >/dev/null
printf '%s\n' "$wwan_call" | grep -F '"pdp_type":"IPV4V6"' >/dev/null
printf '%s\n' "$wwan_call" | grep -F '"profile_id":7' >/dev/null
printf '%s\n' "$wwan_call" | grep -F '"source_module":"WEBUI"' >/dev/null

curl -fsS -H 'Authorization: Bearer fixture-private-token' \
    -H 'Content-Type: application/json' \
    -d '{"action":"wifi.status","params":{}}' \
    "http://127.0.0.1:$PORT/control" | python3 -c \
    'import json,sys; data=json.load(sys.stdin); assert data["result"]["main_2g"]["ssid"] == "Fixture 2G"'

curl -fsS -H 'Authorization: Bearer fixture-private-token' \
    -H 'Content-Type: application/json' \
    -d '{"action":"device.login_info","params":{}}' \
    "http://127.0.0.1:$PORT/control" | python3 -c \
    'import json,sys; data=json.load(sys.stdin); assert data["result"]["zte_web_sault"] == "fixture-salt"'

curl -fsS -H 'Authorization: Bearer fixture-private-token' \
    -H 'Content-Type: application/json' \
    -d '{"action":"device.login","params":{"password_hash":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}' \
    "http://127.0.0.1:$PORT/control" | python3 -c \
    'import json,sys; assert json.load(sys.stdin)["ok"] is True'
curl -fsS -H 'Authorization: Bearer fixture-private-token' \
    -H 'Content-Type: application/json' \
    -d '{"action":"device.session_status","params":{}}' \
    "http://127.0.0.1:$PORT/control" | python3 -c \
    'import json,sys; data=json.load(sys.stdin); assert data["result"]["logged_in"] is True'

curl -fsS -H 'Authorization: Bearer fixture-private-token' \
    -H 'Content-Type: application/json' \
    -d '{"action":"wifi.dual_band_status","params":{}}' \
    "http://127.0.0.1:$PORT/control" | python3 -c \
    'import json,sys; data=json.load(sys.stdin); assert data["result"]["WiFiDualBandEnabled"] == "1"'

curl -fsS -H 'Authorization: Bearer fixture-private-token' \
    -H 'Content-Type: application/json' \
    -d '{"action":"wifi.set_dual_band","params":{"enabled":false}}' \
    "http://127.0.0.1:$PORT/control" | python3 -c \
    'import json,sys; assert json.load(sys.stdin)["ok"] is True'
grep -F 'router_set_wifi_isolate' "$CALL_LOG" | grep -F '"wifimain24_wifimain5_enable":0' | grep -F '"other_option":7' >/dev/null

curl -fsS -H 'Authorization: Bearer fixture-private-token' \
    -H 'Content-Type: application/json' \
    -d '{"action":"lan.set_mtu","params":{"mtu":"1500"}}' \
    "http://127.0.0.1:$PORT/control" | python3 -c \
    'import json,sys; assert json.load(sys.stdin)["ok"] is True'
grep -F 'router_set_wan_mtu' "$CALL_LOG" | grep -F '"wan_mtu":"1500"' >/dev/null

code="$(curl -sS -o "$INVALID_JSON_OUT" -w '%{http_code}' \
    -H 'Authorization: Bearer fixture-private-token' \
    -H 'Content-Type: application/json' \
    --data-binary '{"action":"device.reboot"' \
    "http://127.0.0.1:$PORT/control")"
[ "$code" = "400" ]
python3 -c 'import json,sys; data=json.load(open(sys.argv[1])); assert data["error"]["code"] == "invalid_request"' "$INVALID_JSON_OUT"

python3 - "$PORT" <<'PY'
import socket, sys

request = (
    b"POST /control HTTP/1.1\r\n"
    b"Host: 127.0.0.1\r\n"
    b"Authorization: Bearer fixture-private-token\r\n"
    b"Content-Length: 0\r\n\r\n"
    b'{"action":"device.reboot","params":{}}'
)
with socket.create_connection(("127.0.0.1", int(sys.argv[1]))) as sock:
    sock.sendall(request)
    response = sock.recv(4096)
assert response.startswith(b"HTTP/1.1 400 "), response
PY
! grep -F 'device_reboot' "$CALL_LOG" >/dev/null

code="$(curl -sS -o "$INVALID_PARAM_OUT" -w '%{http_code}' \
    -H 'Authorization: Bearer fixture-private-token' \
    -H 'Content-Type: application/json' \
    -d '{"action":"network.set_mode","params":{}}' \
    "http://127.0.0.1:$PORT/control")"
[ "$code" = "400" ]
python3 -c 'import json,sys; data=json.load(open(sys.argv[1])); assert data["error"]["code"] == "invalid_parameter"' "$INVALID_PARAM_OUT"

code="$(curl -sS -o "$INVALID_WIFI_OUT" -w '%{http_code}' \
    -H 'Authorization: Bearer fixture-private-token' \
    -H 'Content-Type: application/json' \
    -d '{"action":"wifi.configure","params":{"section":"main_2g","ssid":"Must Not Be Staged","enabled":2}}' \
    "http://127.0.0.1:$PORT/control")"
[ "$code" = "400" ]
! grep -F "$(printf 'uci\tset ')" "$CALL_LOG" >/dev/null

code="$(curl -sS -o "$INVALID_WIFI_OUT" -w '%{http_code}' \
    -H 'Authorization: Bearer fixture-private-token' \
    -H 'Content-Type: application/json' \
    -d '{"action":"wifi.configure","params":{"section":"main_2g","ssid":"Staged First","encryption":"__mock_fail__"}}' \
    "http://127.0.0.1:$PORT/control")"
[ "$code" = "502" ]
grep -F "$(printf 'uci\trevert wireless')" "$CALL_LOG" >/dev/null

: >"$MU5252_CALL_LOG"
ZWRT_DATAD_UBUS_BIN="$ROOT/tests/mock_ubus.sh" \
ZWRT_DATAD_UCI_BIN="$ROOT/tests/mock_uci.sh" \
MOCK_CALL_LOG="$MU5252_CALL_LOG" \
MOCK_MODEL_NAME=MU5252 \
MOCK_SIM_SLOT=2 \
MOCK_NWINFO_FAIL=1 \
MOCK_ENCRYPTED_SIM=1 \
ZWRT_DATAD_ADB_BIN="$ROOT/tests/mock_adb.sh" \
ZWRT_DATAD_FAN_PWM_PATH="$COOLING_TMP/pwm1" \
ZWRT_DATAD_FAN_THERMAL_ENABLE_PATH="$COOLING_TMP/fan_thermal_enable" \
ZWRT_DATAD_FAN_COOLING_STATE_PATH="$COOLING_TMP/fan_cooling_state" \
ZWRT_DATAD_LIQUID_THERMAL_ENABLE_PATH="$COOLING_TMP/liquid_thermal_enable" \
ZWRT_DATAD_LIQUID_DRIVE_PATH="$COOLING_TMP/liquid_drive" \
ZWRT_DATAD_COOLING_ZONE_PATH="$COOLING_TMP/zone" \
ZWRT_DATAD_COOLING_CONFIG="$COOLING_TMP/cooling.conf" \
ZWRT_DATAD_ICG_CONFIG="$ROOT/tests/fixtures/icg.conf" \
ZWRT_DATAD_PROC_NET_TCP="$ROOT/tests/fixtures/proc_net_tcp" \
ZWRT_DATAD_ICG_SOCKET_INODES='1,2,3,900,901' \
ZWRT_DATAD_MWAN3_RUNNING=1 \
"$BIN" --once | python3 -c '
import json, sys
data = json.load(sys.stdin)
assert data["device"]["api_template"] == "MU5252"
assert data["device"]["api_template_supported"] == 1
assert data["device"]["full_ubus"] == 1
assert data["net"]["type"] == "SA"
assert data["net"]["operator"] == "Fixture TopFlow Mobile"
assert data["net"]["nr_pci"] == 321
assert data["sim"]["imsi"] == "460000000000001"
assert data["sim"]["msisdn"] == "10086"
assert data["uci_device_info"]["imsi"] == "460000000000001"
assert data["uci_device_info"]["lan_ipaddr"] == "192.168.0.1"
assert data["uci_device_info"]["month_rx_bytes"] == "1000"
assert [modem["id"] for modem in data["modems"]] == ["x75", "v3e1", "v3e2"]
assert [modem["subid"] for modem in data["modems"]] == [2, 3, 5]
assert data["modems"][0]["net"]["nr_pci"] == 321
assert data["modems"][0]["net"]["nr_cell_id"] == 123456
assert data["modems"][1]["net"]["operator"] == "Fixture LTE One"
assert data["modems"][2]["net"]["operator"] == "Fixture LTE Two"
assert "bandwidth" not in data["modems"][1]["net"]
assert data["modems"][2]["net"]["bandwidth"] == "20"
assert data["thermal"]["cpu_celsius"] == 42
assert data["thermal"]["zones"] == [
    {"name": "battery", "celsius": 30.0},
    {"name": "cpuss-0", "celsius": 41.25},
]
assert data["thermal"]["modems"][0] == {
    "id": "x75", "available": True, "celsius": 42
}
assert data["thermal"]["modems"][1]["id"] == "v3e1"
assert data["thermal"]["modems"][1]["available"] is True
assert data["thermal"]["modems"][1]["celsius"] == 47
assert data["thermal"]["modems"][2]["id"] == "v3e2"
assert data["thermal"]["modems"][2]["available"] is True
assert data["thermal"]["modems"][2]["celsius"] == 46
assert data["aggregation"]["enabled"] is True
assert data["aggregation"]["mode"] == "SMULTIWAN"
assert data["aggregation"]["state"] == "online"
assert data["aggregation"]["provisioned"] is True
assert data["aggregation"]["online"] is True
assert data["aggregation"]["controller"] == {
    "icg_process_running": True, "mwan3_running": True
}
assert data["aggregation"]["server"] == {
    "ip": "198.51.100.44", "tcp_port": 10036,
    "udp_start_port": 20000, "source": "runtime"
}
assert data["aggregation"]["tcp_tunnel_count"] == 2
assert data["aggregation"]["path_count"] == 3
assert data["aggregation"]["online_path_count"] == 2
assert data["aggregation"]["traffic"] == {
    "remaining_bytes": 51943409341,
    "remaining_raw": "51943409341",
    "today_used_bytes": 32386704,
    "today_used_raw": "32386704",
}
paths = {path["id"]: path for path in data["aggregation"]["paths"]}
assert paths["x75"]["latency_ms"] == 18.5
assert paths["x75"]["packet_loss_percent"] == 0
assert paths["x75"]["interface_up"] is True
assert paths["v3e1"]["latency_ms"] == 35
assert paths["v3e1"]["packet_loss_percent"] == 1
assert paths["v3e2"]["online"] is False
assert paths["v3e2"]["packet_loss_percent"] == 100
assert "ethernet" not in paths
assert data["multiwan"]["mode"] == "SMULTIWAN"
assert data["multiwan"]["active"] is False
sections = {section["id"]: section for section in data["multiwan"]["sections"]}
assert sections["zte_mwan2"]["track_ip"] == ["1.1.1.1", "8.8.8.8"]
assert sections["zte_mwan2_m1"]["weight"] == "3"
assert sections["balanced"]["use_member"] == ["zte_mwan2_m1"]
assert sections["default_rule_v4"]["use_policy"] == "balanced"
assert data["cooling"]["fan"]["enabled"] is True
assert data["cooling"]["fan"]["always_on"] is True
assert data["cooling"]["fan"]["mode"] == "always_on"
assert data["cooling"]["fan"]["pwm"] == 128
assert data["cooling"]["fan"]["speed_percent"] == 50
assert data["cooling"]["fan"]["manual_speed_percent"] == 0
assert "rpm" not in data["cooling"]["fan"]
assert data["cooling"]["liquid"]["enabled"] is False
assert data["cooling"]["liquid"]["always_on"] is False
assert data["cooling"]["liquid"]["mode"] == "automatic"
assert data["cooling"]["liquid"]["level"] == 0
assert data["cooling"]["liquid"]["speed_percent"] == 0
assert data["cooling"]["liquid"]["levels_percent"] == [30, 100]
assert data["cooling"]["curve"] == [
    {"level": 1, "temperature_celsius": 44, "hysteresis_celsius": 4, "pwm": 76, "speed_percent": 30},
    {"level": 2, "temperature_celsius": 48, "hysteresis_celsius": 4, "pwm": 128, "speed_percent": 50},
    {"level": 3, "temperature_celsius": 53, "hysteresis_celsius": 4, "pwm": 179, "speed_percent": 70},
]
'
grep -F 'get_wwandst' "$MU5252_CALL_LOG" | grep -F '"subid":2' >/dev/null
grep -F 'get_wwandst' "$MU5252_CALL_LOG" | grep -F '"subid":3' >/dev/null
grep -F 'get_wwandst' "$MU5252_CALL_LOG" | grep -F '"subid":5' >/dev/null

# issue #23: when an external modem is camped on local SIM slot 1, its network
# fields are published as msim_<modem>_1_* (not msim_<modem>_0_*). Both the
# realtime UBus path and the UCI fallback must follow v3t_<n>_st_slot.
MOCK_MODEL_NAME=MU5252 MOCK_V3T_SLOT=1 \
ZWRT_DATAD_UBUS_BIN="$ROOT/tests/mock_ubus.sh" \
ZWRT_DATAD_UCI_BIN="$ROOT/tests/mock_uci.sh" \
"$BIN" --once | python3 -c '
import json, sys
data = json.load(sys.stdin)
modems = {m["id"]: m for m in data["modems"]}
assert modems["v3e1"]["subid"] == 4, modems["v3e1"]["subid"]
assert modems["v3e2"]["subid"] == 6, modems["v3e2"]["subid"]
assert modems["v3e1"]["net"]["operator"] == "Fixture LTE One", modems["v3e1"]["net"]
assert modems["v3e2"]["net"]["operator"] == "Fixture LTE Two", modems["v3e2"]["net"]
assert modems["v3e1"]["net"]["band"] == "LTE BAND 3", modems["v3e1"]["net"]
'

# Mixed slots + realtime msim query unavailable -> UCI fallback, still per-modem.
MOCK_MODEL_NAME=MU5252 MOCK_V3T1_SLOT=1 MOCK_V3T2_SLOT=0 MOCK_MSIM_NWINFO_FAIL=1 \
ZWRT_DATAD_UBUS_BIN="$ROOT/tests/mock_ubus.sh" \
ZWRT_DATAD_UCI_BIN="$ROOT/tests/mock_uci.sh" \
"$BIN" --once | python3 -c '
import json, sys
data = json.load(sys.stdin)
modems = {m["id"]: m for m in data["modems"]}
assert modems["v3e1"]["subid"] == 4, modems["v3e1"]["subid"]
assert modems["v3e2"]["subid"] == 5, modems["v3e2"]["subid"]
assert modems["v3e1"]["net"]["operator"] == "Fixture LTE One", modems["v3e1"]["net"]
assert modems["v3e2"]["net"]["operator"] == "Fixture LTE Two", modems["v3e2"]["net"]
'

# Legacy manual/off configurations are no longer a valid steady state: an
# upgrade must move them to the saved custom curve before exposing the switch.
printf '%s\n' \
    'fan_enabled=0' 'fan_auto=0' 'fan_mode=0' 'fan_speed_percent=0' \
    'liquid_enabled=0' 'custom_curve_count=2' \
    'custom_temperature_1=40' 'custom_pwm_1=0' \
    'custom_temperature_2=70' 'custom_pwm_2=255' \
    >"$COOLING_TMP/cooling.conf"
ZWRT_DATAD_UBUS_BIN="$ROOT/tests/mock_ubus.sh" \
ZWRT_DATAD_UCI_BIN="$ROOT/tests/mock_uci.sh" \
MOCK_CALL_LOG="$MU5252_CALL_LOG" \
MOCK_MODEL_NAME=MU5252 \
ZWRT_DATAD_ADB_BIN="$ROOT/tests/mock_adb.sh" \
ZWRT_DATAD_FAN_PWM_PATH="$COOLING_TMP/pwm1" \
ZWRT_DATAD_FAN_THERMAL_ENABLE_PATH="$COOLING_TMP/fan_thermal_enable" \
ZWRT_DATAD_FAN_COOLING_STATE_PATH="$COOLING_TMP/fan_cooling_state" \
ZWRT_DATAD_LIQUID_THERMAL_ENABLE_PATH="$COOLING_TMP/liquid_thermal_enable" \
ZWRT_DATAD_LIQUID_DRIVE_PATH="$COOLING_TMP/liquid_drive" \
ZWRT_DATAD_COOLING_ZONE_PATH="$COOLING_TMP/zone" \
ZWRT_DATAD_COOLING_CONFIG="$COOLING_TMP/cooling.conf" \
"$BIN" --once | python3 -c '
import json, sys
fan = json.load(sys.stdin)["cooling"]["fan"]
assert fan["always_on"] is False
assert fan["mode"] == "custom"
'
grep -F 'fan_mode=2' "$COOLING_TMP/cooling.conf" >/dev/null
grep -F 'fan_always_on=0' "$COOLING_TMP/cooling.conf" >/dev/null

# Simulate the exact 0.9.10 shape: custom mode incorrectly persisted the
# vendor fan switch as enabled and had no explicit always-on key. Startup must
# preserve the user's curve while migrating the switch to always-on=false.
printf '%s\n' \
    'fan_enabled=1' \
    'fan_auto=0' \
    'fan_mode=2' \
    'fan_speed_percent=54' \
    'liquid_enabled=0' \
    'custom_curve_count=5' \
    'custom_temperature_1=40' 'custom_pwm_1=0' \
    'custom_temperature_2=46' 'custom_pwm_2=17' \
    'custom_temperature_3=53' 'custom_pwm_3=59' \
    'custom_temperature_4=59' 'custom_pwm_4=128' \
    'custom_temperature_5=70' 'custom_pwm_5=255' \
    >"$COOLING_TMP/cooling.conf"

ZWRT_DATAD_UBUS_BIN="$ROOT/tests/mock_ubus.sh" \
ZWRT_DATAD_UCI_BIN="$ROOT/tests/mock_uci.sh" \
MOCK_CALL_LOG="$MU5252_CALL_LOG" \
MOCK_MODEL_NAME=MU5252 \
ZWRT_DATAD_ADB_BIN="$ROOT/tests/mock_adb.sh" \
ZWRT_DATAD_FAN_PWM_PATH="$COOLING_TMP/pwm1" \
ZWRT_DATAD_FAN_THERMAL_ENABLE_PATH="$COOLING_TMP/fan_thermal_enable" \
ZWRT_DATAD_FAN_COOLING_STATE_PATH="$COOLING_TMP/fan_cooling_state" \
ZWRT_DATAD_LIQUID_THERMAL_ENABLE_PATH="$COOLING_TMP/liquid_thermal_enable" \
ZWRT_DATAD_LIQUID_DRIVE_PATH="$COOLING_TMP/liquid_drive" \
ZWRT_DATAD_COOLING_ZONE_PATH="$COOLING_TMP/zone" \
ZWRT_DATAD_COOLING_CONFIG="$COOLING_TMP/cooling.conf" \
ZWRT_DATAD_MWAN3_INIT=/usr/bin/true \
"$BIN" -i 200 -p "$TOPFLOW_PORT" --auth-token-file "$TOKEN_FILE" >/dev/null 2>&1 &
TOPFLOW_PID=$!

i=0
until curl -fsS "http://127.0.0.1:$TOPFLOW_PORT/healthz" >/dev/null; do
    i=$((i + 1))
    [ "$i" -lt 50 ] || { echo 'TopFlow test server did not start' >&2; exit 1; }
    sleep 0.1
done

curl -fsS -H 'X-Auth-Token: fixture-private-token' \
    -H 'Content-Type: application/json' \
    -d '{"service":"system","method":"info","args":{}}' \
    "http://127.0.0.1:$TOPFLOW_PORT/ubus/call" | python3 -c '
import json, sys
data = json.load(sys.stdin)
assert data["ok"] is True
assert data["service"] == "system"
assert data["method"] == "info"
assert data["result"]["uptime"] == 123
'

code="$(curl -sS -o /dev/null -w '%{http_code}' \
    -H 'X-Auth-Token: fixture-private-token' \
    -H 'Content-Type: application/json' \
    -d '{"service":"system;reboot","method":"info","args":{}}' \
    "http://127.0.0.1:$TOPFLOW_PORT/ubus/call")"
[ "$code" = "400" ]

curl -fsS -H 'X-Auth-Token: fixture-private-token' \
    "http://127.0.0.1:$TOPFLOW_PORT/capabilities" | \
    grep -F 'cooling.fan.set_curve' >/dev/null
curl -fsS -H 'X-Auth-Token: fixture-private-token' \
    "http://127.0.0.1:$TOPFLOW_PORT/capabilities" | \
    grep -F 'state.set_interval' >/dev/null
curl -fsS -H 'X-Auth-Token: fixture-private-token' \
    "http://127.0.0.1:$TOPFLOW_PORT/capabilities" | \
    grep -F 'cooling.liquid.set_mode' >/dev/null
! curl -fsS -H 'X-Auth-Token: fixture-private-token' \
    "http://127.0.0.1:$TOPFLOW_PORT/capabilities" | \
    grep -F 'cooling.fan.set_speed' >/dev/null

curl -fsS -H 'X-Auth-Token: fixture-private-token' \
    "http://127.0.0.1:$TOPFLOW_PORT/state" | python3 -c '
import json, sys
data = json.load(sys.stdin)
fan = data["cooling"]["fan"]
assert fan["enabled"] is False
assert fan["always_on"] is False
assert fan["mode"] == "custom"
assert fan["pwm"] == 23
assert data["cooling"]["curve"] == [
    {"temperature_celsius": 40, "pwm": 0, "speed_percent": 0},
    {"temperature_celsius": 46, "pwm": 17, "speed_percent": 7},
    {"temperature_celsius": 53, "pwm": 59, "speed_percent": 23},
    {"temperature_celsius": 59, "pwm": 128, "speed_percent": 50},
    {"temperature_celsius": 70, "pwm": 255, "speed_percent": 100},
]
'
grep -F 'fan_always_on=0' "$COOLING_TMP/cooling.conf" >/dev/null
grep -F "$(printf 'uci\tset zwrt_deviceui.Device.fan_switch_status=0')" \
    "$MU5252_CALL_LOG" >/dev/null

rm -f "$COOLING_TMP/fan_cooling_state"
code="$(curl -sS -o /dev/null -w '%{http_code}' \
    -H 'X-Auth-Token: fixture-private-token' \
    -H 'Content-Type: application/json' \
    -d '{"action":"cooling.fan.set_enabled","params":{"enabled":true}}' \
    "http://127.0.0.1:$TOPFLOW_PORT/control")"
[ "$code" = "502" ]
[ "$(cat "$COOLING_TMP/zone/mode")" = "enabled" ]
printf '%s\n' '1' >"$COOLING_TMP/fan_cooling_state"
sleep 0.3

rm -f "$COOLING_TMP/pwm1"
code="$(curl -sS -o /dev/null -w '%{http_code}' \
    -H 'X-Auth-Token: fixture-private-token' \
    -H 'Content-Type: application/json' \
    -d '{"action":"cooling.fan.set_enabled","params":{"enabled":true}}' \
    "http://127.0.0.1:$TOPFLOW_PORT/control")"
[ "$code" = "502" ]
[ "$(cat "$COOLING_TMP/zone/mode")" = "enabled" ]
printf '%s\n' '0' >"$COOLING_TMP/pwm1"
sleep 0.3

curl -fsS -H 'X-Auth-Token: fixture-private-token' \
    -H 'Content-Type: application/json' \
    -d '{"action":"cooling.fan.set_curve","params":{"points":[{"temperature":40,"pwm":0},{"temperature":45,"pwm":0},{"temperature":50,"pwm":76},{"temperature":60,"pwm":128},{"temperature":70,"pwm":255}]}}' \
    "http://127.0.0.1:$TOPFLOW_PORT/control" | python3 -c '
import json, sys
data = json.load(sys.stdin)
assert data["ok"] is True
assert data["result"]["mode"] == "custom"
assert len(data["result"]["points"]) == 5
'
[ "$(cat "$COOLING_TMP/zone/mode")" = "disabled" ]
[ "$(cat "$COOLING_TMP/pwm1")" = "30" ]
[ "$(cat "$COOLING_TMP/fan_thermal_enable")" = "1" ]
[ "$(cat "$COOLING_TMP/fan_cooling_state")" = "0" ]
grep -F 'fan_mode=2' "$COOLING_TMP/cooling.conf" >/dev/null
grep -F 'fan_always_on=0' "$COOLING_TMP/cooling.conf" >/dev/null
grep -F 'custom_curve_count=5' "$COOLING_TMP/cooling.conf" >/dev/null
grep -F 'custom_temperature_5=70' "$COOLING_TMP/cooling.conf" >/dev/null
grep -F 'custom_pwm_5=255' "$COOLING_TMP/cooling.conf" >/dev/null

curl -fsS -H 'X-Auth-Token: fixture-private-token' \
    -H 'Content-Type: application/json' \
    -d '{"action":"cooling.fan.set_enabled","params":{"enabled":true}}' \
    "http://127.0.0.1:$TOPFLOW_PORT/control" | python3 -c '
import json, sys
data = json.load(sys.stdin)
assert data["ok"] is True
assert data["result"]["always_on"] is True
assert data["result"]["mode"] == "always_on"
'
[ "$(cat "$COOLING_TMP/pwm1")" = "128" ]
[ "$(cat "$COOLING_TMP/fan_thermal_enable")" = "1" ]
[ "$(cat "$COOLING_TMP/fan_cooling_state")" = "0" ]
grep -F 'fan_always_on=1' "$COOLING_TMP/cooling.conf" >/dev/null
grep -F "$(printf 'uci\tset zwrt_deviceui.Device.fan_switch_status=1')" \
    "$MU5252_CALL_LOG" >/dev/null

curl -fsS -H 'X-Auth-Token: fixture-private-token' \
    "http://127.0.0.1:$TOPFLOW_PORT/state" | python3 -c '
import json, sys
data = json.load(sys.stdin)
assert data["cooling"]["fan"]["always_on"] is True
assert data["cooling"]["fan"]["mode"] == "always_on"
assert data["cooling"]["fan"]["pwm"] == 128
assert len(data["cooling"]["curve"]) == 5
'

curl -fsS -H 'X-Auth-Token: fixture-private-token' \
    -H 'Content-Type: application/json' \
    -d '{"action":"cooling.fan.set_enabled","params":{"enabled":false}}' \
    "http://127.0.0.1:$TOPFLOW_PORT/control" | python3 -c '
import json, sys
data = json.load(sys.stdin)
assert data["ok"] is True
assert data["result"]["always_on"] is False
assert data["result"]["mode"] == "custom"
'
[ "$(cat "$COOLING_TMP/pwm1")" = "30" ]
[ "$(cat "$COOLING_TMP/fan_thermal_enable")" = "1" ]
[ "$(cat "$COOLING_TMP/fan_cooling_state")" = "0" ]
grep -F 'fan_always_on=0' "$COOLING_TMP/cooling.conf" >/dev/null

printf '%s\n' '80000' >"$COOLING_TMP/zone/temp"
curl -fsS -H 'X-Auth-Token: fixture-private-token' \
    -H 'Content-Type: application/json' \
    -d '{"action":"state.refresh","params":{}}' \
    "http://127.0.0.1:$TOPFLOW_PORT/control" >/dev/null
sleep 0.3
curl -fsS -H 'X-Auth-Token: fixture-private-token' \
    "http://127.0.0.1:$TOPFLOW_PORT/state" | python3 -c '
import json, sys
data = json.load(sys.stdin)
assert data["cooling"]["fan"]["mode"] == "custom"
assert data["cooling"]["fan"]["always_on"] is False
assert data["cooling"]["fan"]["pwm"] == 255
assert data["cooling"]["fan"]["manual_speed_percent"] == 54
assert len(data["cooling"]["curve"]) == 5
assert data["cooling"]["curve"][2] == {
    "temperature_celsius": 50, "pwm": 76, "speed_percent": 30
}
'
printf '%s\n' '47000' >"$COOLING_TMP/zone/temp"

code="$(curl -sS -o /dev/null -w '%{http_code}' \
    -H 'X-Auth-Token: fixture-private-token' \
    -H 'Content-Type: application/json' \
    -d '{"action":"cooling.fan.set_curve","params":{"points":[{"temperature":50,"pwm":120},{"temperature":45,"pwm":150}]}}' \
    "http://127.0.0.1:$TOPFLOW_PORT/control")"
[ "$code" = "400" ]

curl -fsS -H 'X-Auth-Token: fixture-private-token' \
    -H 'Content-Type: application/json' \
    -d '{"action":"cooling.liquid.set_enabled","params":{"enabled":true}}' \
    "http://127.0.0.1:$TOPFLOW_PORT/control" | python3 -c '
import json, sys
data = json.load(sys.stdin)
assert data["ok"] is True
assert data["result"]["enabled"] is True
assert data["result"]["always_on"] is True
'
[ "$(cat "$COOLING_TMP/liquid_drive")" = "1023 60 200" ]
[ "$(cat "$COOLING_TMP/liquid_thermal_enable")" = "0" ]
grep -F 'liquid_always_on=1' "$COOLING_TMP/cooling.conf" >/dev/null
grep -F 'liquid_level=1' "$COOLING_TMP/cooling.conf" >/dev/null

curl -fsS -H 'X-Auth-Token: fixture-private-token' \
    -H 'Content-Type: application/json' \
    -d '{"action":"cooling.liquid.set_mode","params":{"mode":"high"}}' \
    "http://127.0.0.1:$TOPFLOW_PORT/control" | python3 -c '
import json, sys
data = json.load(sys.stdin)
assert data["ok"] is True
assert data["result"]["mode"] == "high"
assert data["result"]["level"] == 2
assert data["result"]["speed_percent"] == 100
'
[ "$(cat "$COOLING_TMP/liquid_drive")" = "1023 200 200" ]
grep -F 'liquid_level=2' "$COOLING_TMP/cooling.conf" >/dev/null

curl -fsS -H 'X-Auth-Token: fixture-private-token' \
    -H 'Content-Type: application/json' \
    -d '{"action":"cooling.liquid.set_enabled","params":{"enabled":false}}' \
    "http://127.0.0.1:$TOPFLOW_PORT/control" | python3 -c '
import json, sys
data = json.load(sys.stdin)
assert data["ok"] is True
assert data["result"]["always_on"] is False
'
[ "$(cat "$COOLING_TMP/liquid_drive")" = "0 0 0" ]
[ "$(cat "$COOLING_TMP/liquid_thermal_enable")" = "1" ]
grep -F 'liquid_always_on=0' "$COOLING_TMP/cooling.conf" >/dev/null

curl -fsS -H 'X-Auth-Token: fixture-private-token' \
    -H 'Content-Type: application/json' \
    -d '{"action":"aggregation.set","params":{"enabled":true}}' \
    "http://127.0.0.1:$TOPFLOW_PORT/control" | python3 -c '
import json, sys
data = json.load(sys.stdin)
assert data["ok"] is True
assert data["result"]["enabled"] is True
'
grep -F 'router_set_wan_mode' "$MU5252_CALL_LOG" | grep -F '"opms_wan_mode":"SMULTIWAN"' >/dev/null

curl -fsS -H 'X-Auth-Token: fixture-private-token' \
    -H 'Content-Type: application/json' \
    -d '{"action":"aggregation.set","params":{"enabled":false}}' \
    "http://127.0.0.1:$TOPFLOW_PORT/control" | python3 -c '
import json, sys
data = json.load(sys.stdin)
assert data["ok"] is True
assert data["result"]["enabled"] is False
'
grep -F 'router_stop_agg_mode' "$MU5252_CALL_LOG" | grep -F '"agg_mode_switch":0' >/dev/null
grep -F 'router_set_wan_mode' "$MU5252_CALL_LOG" | grep -F '"opms_wan_mode":"MULTIWAN"' >/dev/null

curl -fsS -H 'X-Auth-Token: fixture-private-token' \
    -H 'Content-Type: application/json' \
    -d '{"action":"multiwan.interface.set","params":{"section":"zte_mwan2","enabled":1,"track_ip":"1.1.1.1,8.8.8.8","reliability":0,"timeout":5}}' \
    "http://127.0.0.1:$TOPFLOW_PORT/control" | python3 -c '
import json, sys
data = json.load(sys.stdin)
assert data["ok"] is True
assert data["result"]["section"] == "zte_mwan2"
assert data["result"]["applied"] is False
'
grep -F "uci$(printf '\t')delete mwan3.zte_mwan2.track_ip" "$MU5252_CALL_LOG" >/dev/null
grep -F "uci$(printf '\t')add_list mwan3.zte_mwan2.track_ip=1.1.1.1" "$MU5252_CALL_LOG" >/dev/null

curl -fsS -H 'X-Auth-Token: fixture-private-token' \
    -H 'Content-Type: application/json' \
    -d '{"action":"multiwan.member.set","params":{"section":"zte_mwan2_m1","metric":20,"weight":4}}' \
    "http://127.0.0.1:$TOPFLOW_PORT/control" >/dev/null
curl -fsS -H 'X-Auth-Token: fixture-private-token' \
    -H 'Content-Type: application/json' \
    -d '{"action":"multiwan.policy.set","params":{"section":"balanced","last_resort":"default","use_member":"zte_mwan2_m1"}}' \
    "http://127.0.0.1:$TOPFLOW_PORT/control" >/dev/null
curl -fsS -H 'X-Auth-Token: fixture-private-token' \
    -H 'Content-Type: application/json' \
    -d '{"action":"multiwan.rule.set","params":{"section":"default_rule_v4","use_policy":"balanced","sticky":0,"logging":1}}' \
    "http://127.0.0.1:$TOPFLOW_PORT/control" >/dev/null

curl -fsS -H 'X-Auth-Token: fixture-private-token' \
    -H 'Content-Type: application/json' \
    -d '{"action":"state.set_interval","params":{"milliseconds":5000}}' \
    "http://127.0.0.1:$TOPFLOW_PORT/control" | python3 -c '
import json, sys
data = json.load(sys.stdin)
assert data["ok"] is True
assert data["result"]["sample_interval_ms"] == 5000
'
sleep 0.3
curl -fsS -H 'X-Auth-Token: fixture-private-token' \
    "http://127.0.0.1:$TOPFLOW_PORT/state" | python3 -c '
import json, sys
assert json.load(sys.stdin)["sample_interval_ms"] == 5000
'
code="$(curl -sS -o /dev/null -w '%{http_code}' \
    -H 'X-Auth-Token: fixture-private-token' \
    -H 'Content-Type: application/json' \
    -d '{"action":"state.set_interval","params":{"milliseconds":499}}' \
    "http://127.0.0.1:$TOPFLOW_PORT/control")"
[ "$code" = "400" ]

curl -fsS -H 'X-Auth-Token: fixture-private-token' \
    -H 'Content-Type: application/json' \
    -d '{"action":"cooling.liquid.set_enabled","params":{"enabled":true}}' \
    "http://127.0.0.1:$TOPFLOW_PORT/control" >/dev/null
[ "$(cat "$COOLING_TMP/liquid_thermal_enable")" = "0" ]

kill "$TOPFLOW_PID"
wait "$TOPFLOW_PID" || true
TOPFLOW_PID=""
[ "$(cat "$COOLING_TMP/fan_thermal_enable")" = "1" ]
[ "$(cat "$COOLING_TMP/zone/mode")" = "enabled" ]
[ "$(cat "$COOLING_TMP/liquid_thermal_enable")" = "1" ]
[ "$(cat "$COOLING_TMP/liquid_drive")" = "0 0 0" ]

MOCK_MODEL_NAME=MC7523 \
MOCK_NO_NFC=1 \
ZWRT_DATAD_UBUS_BIN="$ROOT/tests/mock_ubus.sh" \
ZWRT_DATAD_UCI_BIN="$ROOT/tests/mock_uci.sh" \
"$BIN" --once | python3 -c '
import json, sys
data = json.load(sys.stdin)
assert data["device"]["api_template"] == "MC7523"
assert data["device"]["api_template_supported"] == 1
assert data["device"]["full_ubus"] == 1
assert data["modems"] == []
assert "battery" not in data
assert "nfc" not in data
assert data["thermal"]["cpu_celsius"] == 42
assert data["thermal"]["zones"] == [
    {"name": "battery", "celsius": 30.0},
    {"name": "cpuss-0", "celsius": 41.25},
]
'

ZWRT_DATAD_UBUS_BIN="$ROOT/tests/mock_ubus.sh" \
ZWRT_DATAD_UCI_BIN="$ROOT/tests/mock_uci.sh" \
MOCK_MODEL_NAME=MC7523 \
"$BIN" -i 200 -p "$MC7523_PORT" --auth-token-file "$TOKEN_FILE" >/dev/null 2>&1 &
MC7523_PID=$!

i=0
until curl -fsS "http://127.0.0.1:$MC7523_PORT/healthz" >/dev/null; do
    i=$((i + 1))
    [ "$i" -lt 50 ] || { echo 'MC7523 test server did not start' >&2; exit 1; }
    sleep 0.1
done

curl -fsS -H 'X-Auth-Token: fixture-private-token' \
    -H 'Content-Type: application/json' \
    -d '{"service":"system","method":"info","args":{}}' \
    "http://127.0.0.1:$MC7523_PORT/ubus/call" | python3 -c '
import json, sys
data = json.load(sys.stdin)
assert data["ok"] is True
assert data["result"]["uptime"] == 123
'

MOCK_MODEL_NAME=MC8532B \
ZWRT_DATAD_UBUS_BIN="$ROOT/tests/mock_ubus.sh" \
ZWRT_DATAD_UCI_BIN="$ROOT/tests/mock_uci.sh" \
"$BIN" --once | python3 -c '
import json, sys
data = json.load(sys.stdin)
assert data["device"]["api_template"] == "MC8532B"
assert data["device"]["api_template_supported"] == 1
assert data["device"]["full_ubus"] == 1
assert data["modems"] == []
assert data["thermal"]["cpu_celsius"] == 42
assert data["thermal"]["zones"] == [
    {"name": "battery", "celsius": 30.0},
    {"name": "cpuss-0", "celsius": 41.25},
]
assert data["thermal"]["modems"] == []
'

ZWRT_DATAD_UBUS_BIN="$ROOT/tests/mock_ubus.sh" \
ZWRT_DATAD_UCI_BIN="$ROOT/tests/mock_uci.sh" \
MOCK_MODEL_NAME=MC8532B \
"$BIN" -i 200 -p "$MC8532B_PORT" --auth-token-file "$TOKEN_FILE" >/dev/null 2>&1 &
MC8532B_PID=$!

i=0
until curl -fsS "http://127.0.0.1:$MC8532B_PORT/healthz" >/dev/null; do
    i=$((i + 1))
    [ "$i" -lt 50 ] || { echo 'MC8532B test server did not start' >&2; exit 1; }
    sleep 0.1
done

curl -fsS -H 'X-Auth-Token: fixture-private-token' \
    -H 'Content-Type: application/json' \
    -d '{"service":"system","method":"info","args":{}}' \
    "http://127.0.0.1:$MC8532B_PORT/ubus/call" | python3 -c '
import json, sys
data = json.load(sys.stdin)
assert data["ok"] is True
assert data["result"]["uptime"] == 123
'

ZWRT_DATAD_UBUS_BIN="$ROOT/tests/mock_ubus.sh" \
ZWRT_DATAD_UCI_BIN="$ROOT/tests/mock_uci.sh" \
MOCK_CALL_LOG="$CALL_LOG" \
"$BIN" -i 200 -p "$LAN_LOCAL_PORT" \
    --lan-bind 127.0.0.1 --lan-port "$LAN_PORT" >/dev/null 2>&1 &
LAN_PID=$!

i=0
until curl -fsS "http://127.0.0.1:$LAN_LOCAL_PORT/healthz" >/dev/null; do
    i=$((i + 1))
    [ "$i" -lt 50 ] || { echo 'LAN test server did not start' >&2; exit 1; }
    sleep 0.1
done

code="$(curl -sS -o /dev/null -w '%{http_code}' "http://127.0.0.1:$LAN_LOCAL_PORT/state")"
[ "$code" = "200" ]
code="$(curl -sS -o /dev/null -w '%{http_code}' "http://127.0.0.1:$LAN_PORT/state")"
[ "$code" = "401" ]

login_json="$(curl -fsS -u admin:fixture-password -X POST \
    "http://127.0.0.1:$LAN_PORT/auth/login")"
access_token="$(printf '%s' "$login_json" | python3 -c \
    'import json,sys; print(json.load(sys.stdin)["access_token"])')"
[ "${#access_token}" = "48" ]

long_basic="$(python3 -c '
import base64
print(base64.b64encode(("u" * 300 + ":password").encode()).decode())
')"
code="$(curl -sS -o /dev/null -w '%{http_code}' -X POST \
    -H "Authorization: Basic $long_basic" \
    "http://127.0.0.1:$LAN_PORT/auth/login")"
[ "$code" = "400" ]

curl -fsS -H "Authorization: Bearer $access_token" \
    "http://127.0.0.1:$LAN_PORT/state" | python3 -m json.tool >/dev/null
curl -fsS "http://127.0.0.1:$LAN_PORT/capabilities?access_token=$access_token" | \
    python3 -m json.tool >/dev/null
curl -fsS -H "Authorization: Bearer $access_token" \
    -H 'Content-Type: application/json' \
    -d '{"action":"state.refresh","params":{}}' \
    "http://127.0.0.1:$LAN_PORT/control" | python3 -c \
    'import json,sys; assert json.load(sys.stdin)["ok"] is True'

code="$(curl -sS -o /dev/null -w '%{http_code}' -X POST \
    "http://127.0.0.1:$LAN_PORT/auth/login?username=admin&password=must-not-be-accepted")"
[ "$code" = "400" ]

exchange_json="$(curl -fsS -X POST \
    -H 'X-Web-Token: fixture-vendor-token' \
    -H 'X-Z-Mode: 0' \
    -H 'X-Z-Tag: zwrt-datad' \
    "http://127.0.0.1:$LAN_PORT/auth/exchange")"
exchange_token="$(printf '%s' "$exchange_json" | python3 -c \
    'import json,sys; print(json.load(sys.stdin)["access_token"])')"
curl -fsS -H "Authorization: Bearer $exchange_token" \
    "http://127.0.0.1:$LAN_PORT/capabilities" | python3 -m json.tool >/dev/null

echo 'integration OK'
