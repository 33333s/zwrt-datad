#!/bin/sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
BIN=$(readlink -f "${1:?usage: tests/service_token_test.sh DATAD_BINARY}")
TEST_DIR=$(mktemp -d /tmp/datad-token-service.XXXXXX)

service_command() {
    ZWRT_DATAD_DIR="$TEST_DIR/data" \
    ZWRT_DATAD_BIN="$BIN" \
    ZWRT_DATAD_UBUS_BIN=/bin/false \
    ZWRT_DATAD_UCI_BIN=/bin/false \
    sh "$ROOT/scripts/service.sh" "$1"
}

cleanup() {
    service_command stop >/dev/null 2>&1 || true
    rm -rf "$TEST_DIR"
}
trap cleanup EXIT HUP INT TERM

mkdir -p "$TEST_DIR/data"
service_command start >/dev/null
echo "service started"
test -f "$TEST_DIR/data/auth.token"
echo "token is regular"
test ! -L "$TEST_DIR/data/auth.token"
echo "token is not symlink"
test "$(stat -c %a "$TEST_DIR/data/auth.token")" = 600
echo "token mode is private"
IFS= read -r datad_test_token < "$TEST_DIR/data/auth.token"
echo "token read"
test "${#datad_test_token}" = 64
echo "token length verified"
case "$datad_test_token" in *[!0-9a-f]*) exit 1 ;; esac
echo "token generated securely"

test "$(curl -s -o /dev/null -w '%{http_code}' \
    http://127.0.0.1:9460/webshell/status)" = 401
test "$(curl -s -o /dev/null -w '%{http_code}' \
    -H "Authorization: Bearer $datad_test_token" \
    http://127.0.0.1:9461/webshell/status)" = 403
curl -fsS -H "Authorization: Bearer $datad_test_token" \
    http://127.0.0.1:9460/webshell/status | grep -q '"enabled":true'
echo "listener policy verified"

service_command stop >/dev/null
echo "service stopped"
rm -f "$TEST_DIR/data/auth.token"
ln -s "$TEST_DIR/should-not-be-created" "$TEST_DIR/data/auth.token"
if service_command start >/dev/null 2>&1; then
    exit 1
fi
test ! -e "$TEST_DIR/should-not-be-created"

echo "service token security OK"
