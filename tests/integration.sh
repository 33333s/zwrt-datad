#!/bin/sh
set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${1:-$ROOT/zwrt-datad-test}"
PORT="${ZWRT_DATAD_TEST_PORT:-19460}"
TOKEN_FILE="$ROOT/tests/auth.token"
CALL_LOG="$ROOT/tests/mock-calls.log"
PID=""

cleanup() {
    [ -n "$PID" ] && kill "$PID" 2>/dev/null || true
    [ -n "$PID" ] && wait "$PID" 2>/dev/null || true
    rm -f "$TOKEN_FILE" "$CALL_LOG"
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

echo 'integration OK'
