#!/bin/sh
set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${1:-$ROOT/zwrt-datad-test}"
PORT="${ZWRT_DATAD_TEST_PORT:-19460}"
TOKEN_FILE="$ROOT/tests/auth.token"
CALL_LOG="$ROOT/tests/mock-calls.log"
INVALID_JSON_OUT="$ROOT/tests/invalid-json.out"
INVALID_PARAM_OUT="$ROOT/tests/invalid-param.out"
INVALID_WIFI_OUT="$ROOT/tests/invalid-wifi.out"
PID=""

cleanup() {
    [ -n "$PID" ] && kill "$PID" 2>/dev/null || true
    [ -n "$PID" ] && wait "$PID" 2>/dev/null || true
    rm -f "$TOKEN_FILE" "$CALL_LOG" "$INVALID_JSON_OUT" "$INVALID_PARAM_OUT" "$INVALID_WIFI_OUT"
}
trap cleanup EXIT INT TERM

printf '%s\n' 'fixture-private-token' >"$TOKEN_FILE"
: >"$CALL_LOG"
chmod +x "$ROOT/tests/mock_ubus.sh" "$ROOT/tests/mock_uci.sh"

ZWRT_DATAD_UBUS_BIN="$ROOT/tests/mock_ubus.sh" \
ZWRT_DATAD_UCI_BIN="$ROOT/tests/mock_uci.sh" \
MOCK_CALL_LOG="$CALL_LOG" \
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

curl -fsS -H 'Authorization: Bearer fixture-private-token' \
    "http://127.0.0.1:$PORT/state" | python3 -m json.tool >/dev/null

curl -fsS -H 'X-Auth-Token: fixture-private-token' \
    "http://127.0.0.1:$PORT/capabilities" | python3 -m json.tool >/dev/null

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

echo 'integration OK'
