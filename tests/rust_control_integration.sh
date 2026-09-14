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
mkdir -p "$TMP/data"

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

echo 'rust control HTTP fixture: PASS'
