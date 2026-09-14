#!/bin/sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
PORT=${RUST_CONTROL_PORT:-19460}
TMP=$(mktemp -d)
PID=
cleanup() {
    [ -z "$PID" ] || kill "$PID" 2>/dev/null || true
    rm -rf "$TMP"
}
trap cleanup EXIT INT TERM

export ZWRT_DATAD_UBUS_BIN="$ROOT/tests/mock_ubus.sh"
export ZWRT_DATAD_UCI_BIN="$ROOT/tests/mock_uci.sh"
export MOCK_CALL_LOG="$TMP/calls.log"
export ZWRT_DATAD_OTA_DISABLE_AUTO=1
export ZWRT_DATAD_MWAN3_INIT=/usr/bin/true
export ZWRT_DATAD_IW_BIN="$ROOT/tests/mock_iw.sh"
export ZWRT_DATAD_HOSTAPD_BIN="$ROOT/tests/mock_hostapd.py"
export ZWRT_DATAD_HOSTAPD_CLI_BIN="$ROOT/tests/mock_hostapd_cli.sh"
export ZWRT_DATAD_WIFI_RUNTIME_DIR="$TMP/wifi-runtime"
export ZWRT_DATAD_VENDOR_WIFI_DIR="$TMP/vendor-wifi"
export ZWRT_DATAD_NET_CLASS_DIR="$TMP/net"
export MOCK_NET_CLASS_DIR="$ZWRT_DATAD_NET_CLASS_DIR"
export ZWRT_DATAD_QOS_LOG="$TMP/key.log"
export ZWRT_DATAD_QOS_LOG_ROTATED="$TMP/key.log.0"
export MOCK_IWINFO_DELAY_FILE="$TMP/iwinfo-delay.count"
export MOCK_IWINFO_DELAY_CALLS=3
export MOCK_UCI_STATE_DIR="$TMP/uci-state"
export ZWRT_DATAD_WIFI_CONFIG="$TMP/datad_wifi"
export ZWRT_DATAD_COOLING_CONFIG="$TMP/cooling.conf"
export ZWRT_DATAD_FAN_PWM_PATH="$TMP/pwm1"
export ZWRT_DATAD_FAN_THERMAL_ENABLE_PATH="$TMP/fan-thermal"
export ZWRT_DATAD_FAN_COOLING_STATE_PATH="$TMP/fan-state"
export ZWRT_DATAD_LIQUID_THERMAL_ENABLE_PATH="$TMP/liquid-thermal"
export ZWRT_DATAD_LIQUID_DRIVE_PATH="$TMP/liquid-drive"
export ZWRT_DATAD_COOLING_ZONE_PATH="$TMP/zone"
mkdir -p "$TMP/data"
mkdir -p "$ZWRT_DATAD_VENDOR_WIFI_DIR" "$ZWRT_DATAD_NET_CLASS_DIR"
printf 'fixture-boot-id\n' >"$TMP/boot-id"
export ZWRT_DATAD_BOOT_ID_PATH="$TMP/boot-id"
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
    curl -fsS -H 'content-type: application/json' --data-binary "$1" \
        "http://127.0.0.1:$PORT/control"
}

post '{"action":"network.set_mode","params":{"mode":"Only_5G"}}' |
    python3 -c 'import json,sys; assert json.load(sys.stdin)["ok"] is True'
post '{"action":"band.set_nr_sa","params":{"bands":"78,79"}}' >/dev/null
post '{"action":"sim.set_slot","params":{"slot":2}}' >/dev/null
post '{"action":"wifi.set_dual_band","params":{"enabled":true}}' >/dev/null
post '{"action":"dns.set","params":{"primary":"1.1.1.1","manual_ipv4":1}}' >/dev/null
post '{"action":"apn.add","params":{"name":"fixture","apn":"internet","auth_mode":0}}' >/dev/null
post '{"action":"traffic.set_limit","params":{"enabled":1,"value":"1024","type":2}}' >/dev/null
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
[ "$(stat -f '%Lp' "$ZWRT_DATAD_WIFI_RUNTIME_DIR/datad_ssid_1.conf")" = 600 ]
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
! kill -0 "$first_hostapd_pid" 2>/dev/null
kill -0 "$second_hostapd_pid"
post '{"action":"wifi.interface.configure","params":{"section":"datad_ssid_1","enabled":false}}' >/dev/null
[ ! -e "$ZWRT_DATAD_NET_CLASS_DIR/wlan4" ]
! kill -0 "$second_hostapd_pid" 2>/dev/null
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
[ "$(cat "$ZWRT_DATAD_FAN_PWM_PATH")" = 30 ]
post '{"action":"cooling.fan.set_enabled","params":{"enabled":true}}' >/dev/null
[ "$(cat "$ZWRT_DATAD_FAN_PWM_PATH")" = 128 ]
post '{"action":"cooling.fan.set_mode","params":{"mode":"automatic"}}' >/dev/null
[ "$(cat "$ZWRT_DATAD_COOLING_ZONE_PATH/mode")" = enabled ]
[ "$(cat "$ZWRT_DATAD_COOLING_ZONE_PATH/trip_point_2_temp")" = 53000 ]
post '{"action":"cooling.liquid.set_mode","params":{"mode":"high"}}' >/dev/null
[ "$(cat "$ZWRT_DATAD_LIQUID_DRIVE_PATH")" = '1023 200 200' ]
post '{"action":"cooling.liquid.set_enabled","params":{"enabled":false}}' >/dev/null
[ "$(cat "$ZWRT_DATAD_LIQUID_DRIVE_PATH")" = '0 0 0' ]
rm -f "$ZWRT_DATAD_FAN_PWM_PATH"
cooling_status=$(curl -sS -o "$TMP/cooling-bad.json" -w '%{http_code}' -H 'content-type: application/json' \
    --data-binary '{"action":"cooling.fan.set_enabled","params":{"enabled":true}}' \
    "http://127.0.0.1:$PORT/control")
[ "$cooling_status" = 502 ]
[ "$(cat "$ZWRT_DATAD_COOLING_ZONE_PATH/mode")" = enabled ]

status=$(curl -sS -o "$TMP/bad.json" -w '%{http_code}' -H 'content-type: application/json' \
    --data-binary '{"action":"band.set_lte","params":{"bands":"1;reboot"}}' \
    "http://127.0.0.1:$PORT/control")
[ "$status" = 400 ]
python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); assert d["error"]["code"]=="invalid_parameter"' "$TMP/bad.json"

curl -fsS "http://127.0.0.1:$PORT/capabilities" |
    python3 -c 'import json,sys; d=json.load(sys.stdin); assert d["control"]==d["controls"]; assert "network.set_mode" in d["control"]; assert "sms.send_raw" not in d["control"]'

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
[ "$(stat -f '%Lp' "$ZWRT_DATAD_WIFI_CONFIG")" = 600 ]
grep -F 'fan_mode=1' "$ZWRT_DATAD_COOLING_CONFIG" >/dev/null
grep -F 'custom_pwm_5=255' "$ZWRT_DATAD_COOLING_CONFIG" >/dev/null
grep -F 'liquid_always_on=0' "$ZWRT_DATAD_COOLING_CONFIG" >/dev/null

echo 'rust control HTTP fixture: PASS'
