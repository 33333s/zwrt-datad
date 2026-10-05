#!/bin/sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
PORT=${RUST_CONTROL_PORT:-19460}
TMP=$(mktemp -d)
PID=
cleanup() {
    [ -z "$PID" ] || kill "$PID" 2>/dev/null || true
    [ -z "$PID" ] || wait "$PID" 2>/dev/null || true
    rm -rf "$TMP"
}
trap cleanup EXIT INT TERM

export ZWRT_DATAD_UBUS_BIN="$ROOT/tests/mock_ubus.sh"
export ZWRT_DATAD_UCI_BIN="$ROOT/tests/mock_uci.sh"
export MOCK_CALL_LOG="$TMP/calls.log"
export ZWRT_DATAD_OTA_DISABLE_AUTO=1
export MOCK_CHARGE_STATUS=4
export ZWRT_DATAD_MWAN3_INIT=/usr/bin/true
export ZWRT_DATAD_IW_BIN="$ROOT/tests/mock_iw.sh"
export ZWRT_DATAD_HOSTAPD_BIN="$ROOT/tests/mock_hostapd.py"
export ZWRT_DATAD_HOSTAPD_CLI_BIN="$ROOT/tests/mock_hostapd_cli.sh"
export ZWRT_DATAD_WIFI_RUNTIME_DIR="$TMP/wifi-runtime"
export ZWRT_DATAD_VENDOR_WIFI_DIR="$TMP/vendor-wifi"
export ZWRT_DATAD_NET_CLASS_DIR="$TMP/net"
export ZWRT_DATAD_NET_CLASS_ROOT="$TMP/state-net"
export ZWRT_DATAD_THERMAL_ROOT="$TMP/state-thermal"
export ZWRT_DATAD_PROC_ROOT="$TMP/proc"
export MOCK_NET_CLASS_DIR="$ZWRT_DATAD_NET_CLASS_DIR"
export ZWRT_DATAD_QOS_LOG="$TMP/key.log"
export ZWRT_DATAD_QOS_LOG_ROTATED="$TMP/key.log.0"
export ZWRT_DATAD_DHCP_LEASES_PATH="$TMP/dhcp.leases"
export MOCK_IWINFO_DELAY_FILE="$TMP/iwinfo-delay.count"
export MOCK_IWINFO_DELAY_CALLS=3
export MOCK_UCI_STATE_DIR="$TMP/uci-state"
export MOCK_SIM_SLOT_FILE="$TMP/sim-slot"
export MOCK_NFC_STATE_FILE="$TMP/nfc-state"
export ZWRT_DATAD_WIFI_CONFIG="$TMP/datad_wifi"
export ZWRT_DATAD_COOLING_CONFIG="$TMP/cooling.conf"
export ZWRT_DATAD_FAN_PWM_PATH="$TMP/pwm1"
export ZWRT_DATAD_FAN_THERMAL_ENABLE_PATH="$TMP/fan-thermal"
export ZWRT_DATAD_FAN_COOLING_STATE_PATH="$TMP/fan-state"
export ZWRT_DATAD_LIQUID_THERMAL_ENABLE_PATH="$TMP/liquid-thermal"
export ZWRT_DATAD_LIQUID_DRIVE_PATH="$TMP/liquid-drive"
export ZWRT_DATAD_COOLING_ZONE_PATH="$TMP/zone"
mkdir -p "$TMP/data"
mkdir -p "$ZWRT_DATAD_VENDOR_WIFI_DIR" "$ZWRT_DATAD_NET_CLASS_DIR" "$ZWRT_DATAD_PROC_ROOT"
mkdir -p "$ZWRT_DATAD_NET_CLASS_ROOT/rmnet_data0/statistics"
mkdir -p "$ZWRT_DATAD_THERMAL_ROOT/thermal_zone0"
printf '1000\n' >"$ZWRT_DATAD_NET_CLASS_ROOT/rmnet_data0/statistics/rx_bytes"
printf '2000\n' >"$ZWRT_DATAD_NET_CLASS_ROOT/rmnet_data0/statistics/tx_bytes"
printf 'cpuss-0\n' >"$ZWRT_DATAD_THERMAL_ROOT/thermal_zone0/type"
printf '42000\n' >"$ZWRT_DATAD_THERMAL_ROOT/thermal_zone0/temp"
printf 'fixture-boot-id\n' >"$TMP/boot-id"
printf '4102444800 00:11:22:33:44:99 192.168.0.99 historical-offline *\n' >"$ZWRT_DATAD_DHCP_LEASES_PATH"
printf '1\n' >"$MOCK_SIM_SLOT_FILE"
printf '0 2\n' >"$MOCK_NFC_STATE_FILE"
export ZWRT_DATAD_BOOT_ID_PATH="$TMP/boot-id"
openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out "$TMP/web-private.pem" 2>/dev/null
openssl pkey -in "$TMP/web-private.pem" -pubout -out "$TMP/web-public.pem" 2>/dev/null
export MOCK_WEB_PUBLIC_KEY_FILE="$TMP/web-public.pem"
for base in wlan0 wlan1; do
    cat >"$ZWRT_DATAD_VENDOR_WIFI_DIR/hostapd-$base.conf" <<'EOF'
driver=nl80211
interface=old
ssid=old
wpa_passphrase=must-not-survive
wpa_key_mgmt=WPA-PSK
vendor_element=kept
EOF
done
mkdir -p "$ZWRT_DATAD_COOLING_ZONE_PATH"
for file in "$ZWRT_DATAD_FAN_PWM_PATH" "$ZWRT_DATAD_FAN_THERMAL_ENABLE_PATH" \
    "$ZWRT_DATAD_FAN_COOLING_STATE_PATH" "$ZWRT_DATAD_LIQUID_THERMAL_ENABLE_PATH" \
    "$ZWRT_DATAD_LIQUID_DRIVE_PATH" "$ZWRT_DATAD_COOLING_ZONE_PATH/mode" \
    "$ZWRT_DATAD_COOLING_ZONE_PATH/temp" \
    "$ZWRT_DATAD_COOLING_ZONE_PATH/trip_point_0_temp" "$ZWRT_DATAD_COOLING_ZONE_PATH/trip_point_0_hyst" \
    "$ZWRT_DATAD_COOLING_ZONE_PATH/trip_point_1_temp" "$ZWRT_DATAD_COOLING_ZONE_PATH/trip_point_1_hyst" \
    "$ZWRT_DATAD_COOLING_ZONE_PATH/trip_point_2_temp" "$ZWRT_DATAD_COOLING_ZONE_PATH/trip_point_2_hyst"; do : >"$file"; done
printf '47000\n' >"$ZWRT_DATAD_COOLING_ZONE_PATH/temp"
printf 'fixture\n' >"$ZWRT_DATAD_QOS_LOG"
printf 'rotated\n' >"$ZWRT_DATAD_QOS_LOG_ROTATED"

"$ROOT/rust/target/debug/zwrt-datad" --bind 127.0.0.1 --port "$PORT" \
    --data-dir "$TMP/data" >"$TMP/server.log" 2>&1 &
PID=$!
i=0
while ! curl -fsS "http://127.0.0.1:$PORT/healthz" >/dev/null; do
    i=$((i + 1)); [ "$i" -lt 400 ] || { cat "$TMP/server.log"; exit 1; }
    sleep 0.05
done

post() {
    post_http_code=$(curl -sS -o "$TMP/last-control.json" -w '%{http_code}' \
        -H 'content-type: application/json' --data-binary "$1" \
        "http://127.0.0.1:$PORT/control")
    case "$post_http_code" in
        2??) cat "$TMP/last-control.json" ;;
        *)
            printf 'control fixture failed (HTTP %s):\n' "$post_http_code" >&2
            cat "$TMP/last-control.json" >&2
            return 1
            ;;
    esac
}
file_mode() {
    stat -c '%a' "$1" 2>/dev/null || stat -f '%Lp' "$1"
}
[ -f "$TMP/data/traffic-history.json" ]
[ "$(file_mode "$TMP/data/traffic-history.json")" = 600 ]
python3 - "$TMP/data/traffic-history.json" <<'PY'
import json, sys
history = json.load(open(sys.argv[1]))
assert history["schema"] == 1 and len(history["days"]) == 1
assert next(iter(history["days"].values()))["bytes"] == 360
assert "secret" not in json.dumps(history)
PY
curl -fsS "http://127.0.0.1:$PORT/state" | python3 -c 'import json,sys; assert "traffic_history" not in json.load(sys.stdin)'
process_gone() {
    pid=$1
    attempt=0
    while kill -0 "$pid" 2>/dev/null; do
        if [ ! -e "$ZWRT_DATAD_PROC_ROOT/$pid/cmdline" ]; then
            return 0
        fi
        attempt=$((attempt + 1))
        [ "$attempt" -lt 50 ] || return 1
        sleep 0.02
    done
}
wait_file_value() {
    path=$1
    expected=$2
    attempt=0
    while [ "$(cat "$path" 2>/dev/null || true)" != "$expected" ]; do
        attempt=$((attempt + 1))
        [ "$attempt" -lt 100 ] || return 1
        sleep 0.01
    done
}

post '{"action":"network.set_mode","params":{"mode":"Only_5G"}}' |
    python3 -c 'import json,sys; assert json.load(sys.stdin)["ok"] is True'
post '{"action":"cellular.set","params":{"roaming":1}}' >/dev/null
python3 - "$MOCK_CALL_LOG" <<'PY'
import json, sys
calls = [line.split('\t', 2) for line in open(sys.argv[1]) if '\tset_wwaniface\t' in line]
assert calls
args = json.loads(calls[-1][2])
assert args == {"source_module":"WEBUI", "cid":1, "roam_enable":1}, args
PY
post '{"action":"band.set_nr_sa","params":{"bands":"78,79"}}' >/dev/null
post '{"action":"sim.set_slot","params":{"slot":2}}' >/dev/null
post '{"action":"wifi.set_dual_band","params":{"enabled":true}}' >/dev/null
post '{"action":"dns.set","params":{"primary":"1.1.1.1","manual_ipv4":1}}' >/dev/null
post '{"action":"apn.add","params":{"name":"fixture","apn":"internet","auth_mode":0}}' >/dev/null
post '{"action":"traffic.set_limit","params":{"enabled":1,"value":"1024","type":2}}' >/dev/null
post '{"action":"traffic.set_clear_day","params":{"day":15,"enabled":0}}' >/dev/null
python3 - "$MOCK_CALL_LOG" <<'PY'
import json, sys
calls = [line.split('\t', 2) for line in open(sys.argv[1]) if '\tset_wwandst_clearday\t' in line]
assert calls and json.loads(calls[-1][2])["enable"] == 0
assert json.loads(calls[-1][2])["clearday"] == 15
PY
post '{"action":"nfc.set","params":{"enabled":true}}' |
    python3 -c 'import json,sys; d=json.load(sys.stdin); assert d["ok"] is True; assert d["result"]=={"supported":True,"enabled":True,"switch":1,"flag":2,"changed":True,"verified":True},d'
[ "$(cat "$MOCK_NFC_STATE_FILE")" = '1 2' ]
for sender in 4G1 4G2 v3e1; do
    code=$(curl -sS -o "$TMP/last-control.json" -w '%{http_code}' -H 'content-type: application/json' \
        --data-binary '{"action":"sms.send_raw","params":{"sender":"'"$sender"'","number":"10086","message_hex":"6D4B8BD5","sms_time":"26;08;27;04;00;00;+;0"}}' \
        "http://127.0.0.1:$PORT/control")
    [ "$code" = 400 ] || { printf '%s SMS: HTTP %s\n' "$sender" "$code" >&2; exit 1; }
    grep -F 'SMS sending is not supported' "$TMP/last-control.json" >/dev/null
done
post '{"action":"sms.send_raw","params":{"sender":"host","number":"10086","message_hex":"6D4B8BD5","sms_time":"26;08;27;04;00;00;+;0"}}' >/dev/null
post '{"action":"sms.send_raw","params":{"sender":"sim2","number":"10086","message_hex":"6D4B8BD5","sms_time":"26;08;27;04;00;00;+;0"}}' >/dev/null
[ "$(cat "$MOCK_SIM_SLOT_FILE")" = 2 ]
post '{"action":"client.rename","params":{"mac":"00:11:22:33:44:55","hostname":"fixture"}}' >/dev/null
post '{"action":"wifi.configure","params":{"section":"main_2g","ssid":"Fixture New","enabled":true}}' >/dev/null
post '{"action":"client.block","params":{"mac":"00:11:22:33:44:55"}}' >/dev/null
post '{"action":"multiwan.interface.set","params":{"section":"zte_mwan2","enabled":1,"track_ip":"1.1.1.1,8.8.8.8","timeout":5}}' >/dev/null
post '{"action":"multiwan.member.set","params":{"section":"zte_mwan2_m1","metric":20,"weight":4}}' >/dev/null
post '{"action":"multiwan.policy.set","params":{"section":"balanced","last_resort":"default","use_member":"zte_mwan2_m1"}}' >/dev/null
post '{"action":"multiwan.rule.set","params":{"section":"default_rule_v4","use_policy":"balanced","sticky":0,"logging":1}}' >/dev/null
post '{"action":"aggregation.set","params":{"enabled":true}}' >/dev/null
post '{"action":"aggregation.set","params":{"enabled":false}}' >/dev/null
post '{"action":"qos.clear","params":{}}' >/dev/null
post '{"action":"wifi.txpower.apply","params":{"band":"2g","percent":90,"limit_dbm":19}}' >/dev/null
post '{"action":"wifi.psm.set","params":{"section":"main_5g","mode":"off"}}' >/dev/null
post '{"action":"wifi.txpower.set_dbm","params":{"band":"5g","dbm":17}}' >/dev/null
wireless_status=$(curl -sS -o "$TMP/wireless-bad.json" -w '%{http_code}' -H 'content-type: application/json' \
    --data-binary '{"action":"wireless.config","params":{"band":"5g","channel":100}}' \
    "http://127.0.0.1:$PORT/control")
[ "$wireless_status" = 400 ]
post '{"action":"wireless.config","params":{"band":"5g","channel":149}}' >/dev/null
printf '0\n' >"$MOCK_IWINFO_DELAY_FILE"
post '{"action":"wireless.config","params":{"band":"5g","country":"HK","channel":100}}' >/dev/null
post '{"action":"wifi.interface.create","params":{"band":"5g","ssid":"Fixture Extra","key":"fixture-extra-key"}}' >/dev/null
[ -d "$ZWRT_DATAD_NET_CLASS_DIR/wlan4" ]
first_hostapd_pid=$(cat "$ZWRT_DATAD_WIFI_RUNTIME_DIR/datad_ssid_1.pid")
kill -0 "$first_hostapd_pid"
[ "$(file_mode "$ZWRT_DATAD_WIFI_RUNTIME_DIR/datad_ssid_1.conf")" = 600 ]
grep -F 'ssid=Fixture Extra' "$ZWRT_DATAD_WIFI_RUNTIME_DIR/datad_ssid_1.conf" >/dev/null
grep -F 'wpa_passphrase=fixture-extra-key' "$ZWRT_DATAD_WIFI_RUNTIME_DIR/datad_ssid_1.conf" >/dev/null
grep -F 'vendor_element=kept' "$ZWRT_DATAD_WIFI_RUNTIME_DIR/datad_ssid_1.conf" >/dev/null
! grep -F 'must-not-survive' "$ZWRT_DATAD_WIFI_RUNTIME_DIR/datad_ssid_1.conf" >/dev/null
printf 'old unbounded log\n' >"$ZWRT_DATAD_WIFI_RUNTIME_DIR/datad_ssid_1.log"
post '{"action":"wifi.interface.configure","params":{"section":"datad_ssid_1","ssid":"Fixture Extra 2","enabled":true}}' >/dev/null
[ ! -s "$ZWRT_DATAD_WIFI_RUNTIME_DIR/datad_ssid_1.log" ]
grep -F 'ssid=Fixture Extra 2' "$ZWRT_DATAD_WIFI_RUNTIME_DIR/datad_ssid_1.conf" >/dev/null
second_hostapd_pid=$(cat "$ZWRT_DATAD_WIFI_RUNTIME_DIR/datad_ssid_1.pid")
[ "$second_hostapd_pid" != "$first_hostapd_pid" ]
process_gone "$first_hostapd_pid"
kill -0 "$second_hostapd_pid"
post '{"action":"wifi.interface.configure","params":{"section":"datad_ssid_1","enabled":false}}' >/dev/null
[ ! -e "$ZWRT_DATAD_NET_CLASS_DIR/wlan4" ]
process_gone "$second_hostapd_pid"
mkdir -p "$ZWRT_DATAD_NET_CLASS_DIR/wlan4"
printf '999\n' >"$ZWRT_DATAD_NET_CLASS_DIR/wlan4/ifindex"
extra_failure=$(curl -sS -o "$TMP/extra-failure.json" -w '%{http_code}' -H 'content-type: application/json' \
    --data-binary '{"action":"wifi.interface.configure","params":{"section":"datad_ssid_1","ssid":"Must Roll Back","enabled":true}}' \
    "http://127.0.0.1:$PORT/control")
[ "$extra_failure" = 502 ]
[ "$("$ZWRT_DATAD_UCI_BIN" -q get datad_wifi.datad_ssid_1.ssid)" = 'Fixture Extra 2' ]
[ "$("$ZWRT_DATAD_UCI_BIN" -q get datad_wifi.datad_ssid_1.disabled)" = 1 ]
[ "$(cat "$ZWRT_DATAD_NET_CLASS_DIR/wlan4/ifindex")" = 999 ]
rm -rf "$ZWRT_DATAD_NET_CLASS_DIR/wlan4"
post '{"action":"wifi.interface.delete","params":{"section":"datad_ssid_1"}}' >/dev/null
post '{"action":"cooling.fan.set_curve","params":{"points":[{"temperature":40,"pwm":0},{"temperature":45,"pwm":0},{"temperature":50,"pwm":76},{"temperature":60,"pwm":128},{"temperature":70,"pwm":255}]}}' >/dev/null
wait_file_value "$ZWRT_DATAD_FAN_PWM_PATH" 30
post '{"action":"cooling.fan.set_enabled","params":{"enabled":true}}' >/dev/null
wait_file_value "$ZWRT_DATAD_FAN_PWM_PATH" 128
post '{"action":"cooling.fan.set_mode","params":{"mode":"automatic"}}' >/dev/null
wait_file_value "$ZWRT_DATAD_COOLING_ZONE_PATH/mode" enabled
wait_file_value "$ZWRT_DATAD_COOLING_ZONE_PATH/trip_point_2_temp" 53000
post '{"action":"cooling.liquid.set_mode","params":{"mode":"high"}}' >/dev/null
wait_file_value "$ZWRT_DATAD_LIQUID_DRIVE_PATH" '1023 200 200'
post '{"action":"cooling.liquid.set_enabled","params":{"enabled":false}}' >/dev/null
wait_file_value "$ZWRT_DATAD_LIQUID_DRIVE_PATH" '0 0 0'
rm -f "$ZWRT_DATAD_FAN_PWM_PATH"
cooling_status=$(curl -sS -o "$TMP/cooling-bad.json" -w '%{http_code}' -H 'content-type: application/json' \
    --data-binary '{"action":"cooling.fan.set_enabled","params":{"enabled":true}}' \
    "http://127.0.0.1:$PORT/control")
[ "$cooling_status" = 502 ]
wait_file_value "$ZWRT_DATAD_COOLING_ZONE_PATH/mode" enabled

status=$(curl -sS -o "$TMP/bad.json" -w '%{http_code}' -H 'content-type: application/json' \
    --data-binary '{"action":"band.set_lte","params":{"bands":"1;reboot"}}' \
    "http://127.0.0.1:$PORT/control")
[ "$status" = 400 ]
python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); assert d["error"]["code"]=="invalid_parameter"' "$TMP/bad.json"

# Hosts writes must fail at confirmation before touching the fixed file.
for hosts_action in hosts.save hosts.restore; do
    hosts_status=$(curl -sS -o "$TMP/hosts-unconfirmed.json" -w '%{http_code}' -H 'content-type: application/json' \
        --data-binary "{\"action\":\"$hosts_action\",\"params\":{},\"confirmed\":false}" \
        "http://127.0.0.1:$PORT/control")
    [ "$hosts_status" = 400 ]
    python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); assert d["ok"] is False and d["error"]["code"]=="invalid_parameter"' "$TMP/hosts-unconfirmed.json"
done
curl -fsS "http://127.0.0.1:$PORT/state" | python3 -c 'import json,sys; assert "hosts_config" not in json.load(sys.stdin)'

curl -fsS "http://127.0.0.1:$PORT/capabilities" |
    python3 -c 'import json,sys; d=json.load(sys.stdin); assert d["control"]==d["controls"]; assert len(d["control"])==len(set(d["control"]))==87+3+3+2, d["control"]; assert {"time.status","time.config.set","time.sync","hosts.status","hosts.save","hosts.restore","device.session.login","datad.ota.set"} <= set(d["control"]); assert "sms.forward.set" in d["control"] and "sms.forward.test" in d["control"]; assert "schedule.task.put" in d["control"] and "schedule.task.remove" in d["control"]; assert "cloud.remote_features.set" in d["control"]; assert "schedule.reboot.set" in d["control"]; assert "speedtest.start" in d["control"] and "speedtest.stop" in d["control"]; assert "network.set_mode" in d["control"]; assert "sms.send_raw" in d["control"]; assert d["discovery"]==["ubus.list","ubus.list_verbose"]; assert d["passthrough"]==["ubus.call"]; assert d["transport"]==["http","sse"]'
post '{"action":"sms.forward.set","params":{"enabled":false,"method":"webhook","webhook_url":"https://example.com/hook"}}' |
    python3 -c 'import json,sys; d=json.load(sys.stdin); assert d["ok"] is True and d["result"]["enabled"] is False and d["result"]["webhook_configured"] is True; assert "webhook_url" not in d["result"]'
[ "$(file_mode "$TMP/data/sms-forward.json")" = 600 ]
post '{"action":"sms.forward.set","params":{"enabled":false,"method":"sms","sms_to_phone":["+10086"]}}' |
    python3 -c 'import json,sys; d=json.load(sys.stdin); assert d["ok"] is True and d["result"]["sms_configured"] is True; assert "sms_to_phone" not in d["result"] and "10086" not in json.dumps(d)'
post '{"action":"sms.forward.test","params":{}}' |
    python3 -c 'import json,sys; d=json.load(sys.stdin); assert d["ok"] is True and d["result"]["sms_daily_remaining"] == 59'
post '{"action":"sms.forward.set","params":{"enabled":false,"method":"sms","sms_to_phone":[]}}' |
    python3 -c 'import json,sys; d=json.load(sys.stdin); assert d["ok"] is True and d["result"]["sms_configured"] is False and d["result"]["sms_daily_remaining"] == 59'
post '{"action":"sms.forward.set","params":{"enabled":false,"method":"smtp","smtp":{"host":"smtp.example.com","port":465,"username":"sender@example.com","password":"fixture-app-password","to":"recipient@example.net"}}}' |
    python3 -c 'import json,sys; d=json.load(sys.stdin); assert d["ok"] is True and d["result"]["smtp_configured"] is True and d["result"]["enabled"] is False; assert "fixture-app-password" not in json.dumps(d) and "recipient@example.net" not in json.dumps(d)'
[ "$(file_mode "$TMP/data/sms-forward.json")" = 600 ]
post '{"action":"sms.forward.set","params":{"enabled":false,"method":"smtp","smtp":{"host":"","port":0,"username":"","password":"","to":""}}}' |
    python3 -c 'import json,sys; d=json.load(sys.stdin); assert d["ok"] is True and d["result"]["smtp_configured"] is False'
post '{"action":"sms.forward.set","params":{"enabled":false,"method":"smtp","power_forward_enabled":true}}' |
    python3 -c 'import json,sys; d=json.load(sys.stdin); assert d["ok"] is True and d["result"]["power_supported"] is True and d["result"]["power_forward_enabled"] is True and d["result"]["power_daily_remaining"] == 60 and d["result"]["enabled"] is False'
post '{"action":"sms.forward.set","params":{"enabled":false,"method":"smtp","power_forward_enabled":false}}' |
    python3 -c 'import json,sys; d=json.load(sys.stdin); assert d["ok"] is True and d["result"]["power_forward_enabled"] is False and d["result"]["power_daily_remaining"] == 60'
post '{"action":"sms.forward.set","params":{"enabled":false,"method":"smtp","blacklist_phone":["10086"],"blacklist_keywords":["验证码"]}}' |
    python3 -c 'import json,sys; d=json.load(sys.stdin); assert d["ok"] is True and d["result"]["rules_supported"] is True and d["result"]["blacklist_phone_count"] == 1 and d["result"]["blacklist_keywords_count"] == 1 and "10086" not in json.dumps(d) and "验证码" not in json.dumps(d)'
post '{"action":"sms.forward.set","params":{"enabled":false,"method":"smtp","blacklist_phone":[],"blacklist_keywords":[]}}' |
    python3 -c 'import json,sys; d=json.load(sys.stdin); assert d["ok"] is True and d["result"]["blacklist_phone_count"] == 0 and d["result"]["blacklist_keywords_count"] == 0'
post '{"action":"sms.forward.set","params":{"enabled":false,"method":"smtp","nickname":"客厅 U60"}}' |
    python3 -c 'import json,sys; d=json.load(sys.stdin); assert d["ok"] is True and d["result"]["nickname_supported"] is True and d["result"]["nickname"] == "客厅 U60" and d["result"]["enabled"] is False'
post '{"action":"sms.forward.set","params":{"enabled":false,"method":"smtp","nickname":""}}' |
    python3 -c 'import json,sys; d=json.load(sys.stdin); assert d["ok"] is True and d["result"]["nickname"] == "" and d["result"]["enabled"] is False'
post '{"action":"sms.forward.set","params":{"enabled":false,"method":"smtp","smtp_forward_device_info":true}}' |
    python3 -c 'import json,sys; d=json.load(sys.stdin); assert d["ok"] is True and d["result"]["device_info_supported"] is True and d["result"]["smtp_forward_device_info"] is True and d["result"]["enabled"] is False; assert "device_info" not in d["result"]'
post '{"action":"sms.forward.set","params":{"enabled":false,"method":"dingtalk","dingtalk_forward_device_info":true}}' |
    python3 -c 'import json,sys; d=json.load(sys.stdin); assert d["ok"] is True and d["result"]["dingtalk_forward_device_info"] is True and d["result"]["smtp_forward_device_info"] is True and d["result"]["enabled"] is False'
post '{"action":"sms.forward.set","params":{"enabled":false,"method":"sms","sms_forward_device_info":true}}' |
    python3 -c 'import json,sys; d=json.load(sys.stdin); assert d["ok"] is True and d["result"]["sms_forward_device_info"] is True and d["result"]["enabled"] is False'
post '{"action":"sms.forward.set","params":{"enabled":false,"method":"sms","sms_forward_device_info":false,"smtp_forward_device_info":false,"dingtalk_forward_device_info":false}}' |
    python3 -c 'import json,sys; d=json.load(sys.stdin); assert d["ok"] is True and all(d["result"][key] is False for key in ("sms_forward_device_info","smtp_forward_device_info","dingtalk_forward_device_info"))'
[ "$(file_mode "$TMP/data/sms-forward.json")" = 600 ]
status=$(curl -sS -o "$TMP/bad-forward.json" -w '%{http_code}' -H 'content-type: application/json' \
    --data-binary '{"action":"sms.forward.set","params":{"enabled":true,"method":"webhook","webhook_url":"http://127.0.0.1/private"}}' \
    "http://127.0.0.1:$PORT/control")
[ "$status" = 400 ]
curl -fsS "http://127.0.0.1:$PORT/state" |
    python3 -c 'import json,sys; d=json.load(sys.stdin); assert "sms_forward" not in d'
post '{"action":"speedtest.stop","params":{}}' |
    python3 -c 'import json,sys; d=json.load(sys.stdin); assert d["ok"] is True and d["result"]["provider"]=="cloudflare"'
status=$(curl -sS -o "$TMP/bad-speedtest.json" -w '%{http_code}' -H 'content-type: application/json' \
    --data-binary '{"action":"speedtest.start","params":{"bytes":52428801,"threads":1,"runs":1,"url":"http://127.0.0.1"}}' \
    "http://127.0.0.1:$PORT/control")
[ "$status" = 400 ]
curl -fsS "http://127.0.0.1:$PORT/state" |
    python3 -c 'import json,sys; d=json.load(sys.stdin); assert d["speedtest"]["provider"]=="cloudflare" and d["speedtest"]["requested_bytes"]==0'
post '{"action":"schedule.task.put","params":{"id":"fixture","time":"23:59","repeat_daily":false,"action":"nfc.set","params":{"enabled":true}}}' |
    python3 -c 'import json,sys; d=json.load(sys.stdin); assert d["ok"] is True and d["result"]["tasks"][0]["id"]=="fixture"'
[ "$(file_mode "$TMP/data/scheduled-tasks.json")" = 600 ]
status=$(curl -sS -o "$TMP/bad-task.json" -w '%{http_code}' -H 'content-type: application/json' \
    --data-binary '{"action":"schedule.task.put","params":{"id":"bad","time":"23:59","repeat_daily":false,"action":"ubus.call","params":{"service":"zwrt_web"}}}' \
    "http://127.0.0.1:$PORT/control")
[ "$status" = 400 ]
post '{"action":"schedule.task.remove","params":{"id":"fixture"}}' |
    python3 -c 'import json,sys; d=json.load(sys.stdin); assert d["ok"] is True and d["result"]["tasks"]==[]'
post '{"action":"schedule.reboot.set","params":{"enabled":false,"time":"02:03"}}' |
    python3 -c 'import json,sys; d=json.load(sys.stdin); assert d["ok"] is True and d["result"]["enabled"] is False and d["result"]["time"]=="02:03"'
[ "$(file_mode "$TMP/data/reboot-schedule.json")" = 600 ]
status=$(curl -sS -o "$TMP/bad-schedule.json" -w '%{http_code}' -H 'content-type: application/json' \
    --data-binary '{"action":"schedule.reboot.set","params":{"enabled":true,"time":"02:03","extra":"reject"}}' \
    "http://127.0.0.1:$PORT/control")
[ "$status" = 400 ]
mkdir -p "$MOCK_UCI_STATE_DIR"
printf '1\n' >"$MOCK_UCI_STATE_DIR/zwrt_zte_mc.reboot_schedule.reboot_schedule_enable"
status=$(curl -sS -o "$TMP/conflict-schedule.json" -w '%{http_code}' -H 'content-type: application/json' \
    --data-binary '{"action":"schedule.reboot.set","params":{"enabled":true,"time":"02:03"}}' \
    "http://127.0.0.1:$PORT/control")
[ "$status" = 400 ]
rm -f "$MOCK_UCI_STATE_DIR/zwrt_zte_mc.reboot_schedule.reboot_schedule_enable"
sleep 1.2
curl -fsS "http://127.0.0.1:$PORT/state" |
    python3 -c 'import json,sys; d=json.load(sys.stdin); assert d["reboot_schedule"]["enabled"] is False and d["reboot_schedule"]["time"]=="02:03"'
curl -fsS "http://127.0.0.1:$PORT/state" |
    python3 -c 'import json,sys; d=json.load(sys.stdin); assert d["runtime"]["thermal_zones"]==[{"type":"cpuss-0","temp_milli":42000}]; assert len(d["runtime"]["link_rates"])==1; assert d["runtime"]["link_rates"][0]["interface"]=="rmnet_data0"; assert d["thermal"]["zones"]==[{"name":"cpuss-0","celsius":42.0}]; assert d["thermal"]["protection"]=={"active":True,"level":2,"speed_limited":True,"network_restricted":False,"raw":"1"}; assert d["battery"]["charge_protection"]=={"active":True,"mode":2}; assert d["net"]["HSR"] is True; assert d["net"]["high_speed_rail"]=={"active":True,"raw":"1"}; assert d["nfc"]["switch"]==1; assert d["sms"]["list"][0]["text"]=="测试"; assert d["sms"]["list"][0]["unread"]==1; assert d["net"]["lte_bands"]=="1,3",d["net"]; assert d["net"]["roaming_allowed"]==0,d["net"]; assert d["net"]["lte_tac"]==40302,d["net"]; assert d["net"]["nr_tac"]==1234567,d["net"]; assert d["net"]["lte_supported_bands"]=="1,2,3,7,8,20,28,38,40,41,66",d["net"]; assert d["net"]["band_capabilities"]=={"source":"device_default_band_lock","complete":True,"lte":[1,2,3,7,8,20,28,38,40,41,66],"nr_sa":[1,3,28,41,77,78,79],"nr_nsa":[1,3,28,41,77,78,79]},d["net"]; assert d["clients"]=={"total":2,"wifi":1,"lan":1,"list":[{"name":"wifi-live","ip":"192.168.0.2","mac":"00:11:22:33:44:55"},{"name":"lan-live","ip":"192.168.0.3","mac":"00:11:22:33:44:66"}],"blocked":[]}; assert d["dhcp"]["netmask"]=="255.255.255.0" and d["dhcp"]["disabled"] is False and d["dhcp"]["range_start"]=="192.168.0.2" and d["dhcp"]["range_end"]=="192.168.0.253"'
event_json=$({ curl -sN --max-time 3 "http://127.0.0.1:$PORT/events" || true; } | sed -n 's/^data: //p' | head -n 1)
printf '%s' "$event_json" |
    python3 -c 'import json,sys; d=json.load(sys.stdin); assert d["thermal"]["protection"]["level"]==2; assert d["battery"]["charge_protection"]["active"] is True; assert d["net"]["high_speed_rail"]["active"] is True'
unknown_status=$(curl -sS -o "$TMP/unknown.json" -w '%{http_code}' -H 'content-type: application/json' \
    --data-binary '{"action":"fixture.unknown","params":{}}' "http://127.0.0.1:$PORT/control")
[ "$unknown_status" = 404 ]
python3 -c 'import json,sys; assert json.load(open(sys.argv[1]))["error"]["code"]=="unknown_action"' "$TMP/unknown.json"

python3 - "$MOCK_CALL_LOG" <<'PY'
import json, sys
rows = [line.rstrip("\n").split("\t", 2) for line in open(sys.argv[1])]
calls = {(service, method, json.dumps(json.loads(args), sort_keys=True, separators=(",", ":")))
         for row in rows if len(row) == 3 for service, method, args in [row] if service != "uci"}
expected = {
    ("zte_nwinfo_api", "nwinfo_set_netselect", '{"net_select":"Only_5G"}'),
    ("zte_nwinfo_api", "nwinfo_set_nrbandlock", '{"nr5g_band":"78,79","nr5g_type":"0"}'),
    ("zwrt_router.api", "router_set_lan_dns", '{"dns1":"1.1.1.1","lan_dns_manual_enable":1}'),
    ("zwrt_router.api", "router_modify_lan_hostname", '{"hostname":"fixture","mac":"00:11:22:33:44:55"}'),
}
assert expected <= calls, expected - calls
registration = next(json.loads(row[2])["web_enstr"] for row in rows if len(row) == 3 and row[0] == "zwrt_web" and row[1] == "web_http_enstr_set")
import base64
assert len(base64.b64decode(registration)) == 256
sends = [json.loads(row[2]) for row in rows if len(row) == 3 and row[0] == "zwrt_wms" and row[1] == "zte_libwms_send_sms"]
assert len(sends) == 3
for send in sends:
    assert len(base64.b64decode(send["number"])) > 28
    assert len(base64.b64decode(send["message_body"])) > 28
    assert "10086" not in send["number"] and "6D4B8BD5" not in send["message_body"]
PY
! grep -F '1;reboot' "$MOCK_CALL_LOG" >/dev/null
grep -F 'wireless.main_2g.ssid=Fixture New' "$MOCK_CALL_LOG" >/dev/null
grep -F 'wireless.main_2g.denymaclist=00:11:22:33:44:55' "$MOCK_CALL_LOG" >/dev/null
grep -F 'mwan3.zte_mwan2.timeout=5' "$MOCK_CALL_LOG" >/dev/null
grep -F 'mwan3.zte_mwan2.track_ip=8.8.8.8' "$MOCK_CALL_LOG" >/dev/null
grep -F 'mwan3.zte_mwan2_m1.weight=4' "$MOCK_CALL_LOG" >/dev/null
grep -F 'mwan3.balanced.use_member=zte_mwan2_m1' "$MOCK_CALL_LOG" >/dev/null
grep -F 'mwan3.default_rule_v4.use_policy=balanced' "$MOCK_CALL_LOG" >/dev/null
[ ! -s "$ZWRT_DATAD_QOS_LOG" ]
[ ! -s "$ZWRT_DATAD_QOS_LOG_ROTATED" ]
grep -F 'wireless.wifi0.txpowerpercent=90' "$MOCK_CALL_LOG" >/dev/null
grep -F 'wireless.wifi0.txpower=19' "$MOCK_CALL_LOG" >/dev/null
grep -F 'wireless.wifi0.max_power=19' "$MOCK_CALL_LOG" >/dev/null
grep -F 'wireless.main_5g.datad_psm=off' "$MOCK_CALL_LOG" >/dev/null
grep -F 'iw' "$MOCK_CALL_LOG" | grep -F 'dev wlan0 set power_save off' >/dev/null
grep -F 'wireless.wifi1.datad_txpower_dbm=17' "$MOCK_CALL_LOG" >/dev/null
! grep -F 'wireless.wifi1.channel=100' "$MOCK_CALL_LOG" >/dev/null
grep -F 'wireless.wifi1.channel=149' "$MOCK_CALL_LOG" >/dev/null
grep -F 'wireless.wifi0.country=HK' "$MOCK_CALL_LOG" >/dev/null
grep -F 'wireless.wifi1.country=HK' "$MOCK_CALL_LOG" >/dev/null
grep -F 'wireless.wifi1.channel=0' "$MOCK_CALL_LOG" >/dev/null
grep -F 'wireless.wifi1.channel=100' "$MOCK_CALL_LOG" >/dev/null
[ "$(cat "$MOCK_IWINFO_DELAY_FILE")" -gt 3 ]
grep -F 'datad_wifi.datad_ssid_1=wifi-iface' "$MOCK_CALL_LOG" >/dev/null
grep -F 'datad_wifi.datad_ssid_1.ssid=Fixture Extra 2' "$MOCK_CALL_LOG" >/dev/null
grep -F 'delete datad_wifi.datad_ssid_1' "$MOCK_CALL_LOG" >/dev/null
[ "$(file_mode "$ZWRT_DATAD_WIFI_CONFIG")" = 600 ]
grep -F 'fan_mode=1' "$ZWRT_DATAD_COOLING_CONFIG" >/dev/null
grep -F 'custom_pwm_5=255' "$ZWRT_DATAD_COOLING_CONFIG" >/dev/null
grep -F 'liquid_always_on=0' "$ZWRT_DATAD_COOLING_CONFIG" >/dev/null

echo 'rust control HTTP fixture: PASS'
