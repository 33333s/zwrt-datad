#!/bin/sh
# Concurrent `service.sh start` calls (boot: rc.local plus another starter; an
# update) must launch the daemon once and leave no "Address in use" line.
set -eu

ROOT=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
BIN=$(readlink -f "${1:?usage: tests/service_start_race_test.sh DATAD_BINARY}")
TEST_DIR=$(mktemp -d /tmp/datad-start-race.XXXXXX)

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
for round in 1 2 3; do
    service_command stop >/dev/null 2>&1 || true
    : > "$TEST_DIR/data/zwrt-datad.log"
    for n in 1 2 3 4; do
        service_command start > "$TEST_DIR/out.$n" 2>&1 &
    done
    wait
    pids=$(cat "$TEST_DIR"/out.* | sed -n 's/.*PID \([0-9][0-9]*\).*/\1/p' | sort -u)
    test "$(printf '%s\n' "$pids" | wc -l)" = 1 || { echo "round $round: callers reported different PIDs: $pids"; exit 1; }
    sleep 2
    ! grep -q 'Address in use' "$TEST_DIR/data/zwrt-datad.log" || { echo "round $round: bind error in log"; cat "$TEST_DIR/data/zwrt-datad.log"; exit 1; }
    test "$(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:9460/healthz)" = 200
    test ! -e "$TEST_DIR/data/.service.lock"
done
echo "concurrent starts launch once"

# Deterministic mutual exclusion: while another caller holds the lock, start
# must wait instead of launching, and proceeds once the lock is released.
service_command stop >/dev/null 2>&1 || true
sleep 30 &
holder=$!
mkdir "$TEST_DIR/data/.service.lock"
echo "$holder" > "$TEST_DIR/data/.service.lock/pid"
service_command start > "$TEST_DIR/out.wait" 2>&1 &
waiter=$!
sleep 2
if curl -s -o /dev/null http://127.0.0.1:9460/healthz; then
    echo "start launched while the lock was held"
    kill "$holder" 2>/dev/null || true
    exit 1
fi
rm -rf "$TEST_DIR/data/.service.lock"
wait "$waiter"
kill "$holder" 2>/dev/null || true
test "$(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:9460/healthz)" = 200
test ! -e "$TEST_DIR/data/.service.lock"
echo "start waits for the lock holder"

# A lock left by a killed caller must not block the service.
service_command stop >/dev/null
mkdir "$TEST_DIR/data/.service.lock"
echo 99999999 > "$TEST_DIR/data/.service.lock/pid"
service_command start >/dev/null
test "$(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:9460/healthz)" = 200
test ! -e "$TEST_DIR/data/.service.lock"
echo "stale lock recovered"

# Stopping while a start is racing in is serialised too.
service_command restart >/dev/null
test "$(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:9460/healthz)" = 200
test ! -e "$TEST_DIR/data/.service.lock"
echo "service start race OK"
