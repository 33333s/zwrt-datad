#!/bin/sh
set -eu

if [ "${MOCK_ASSERT_NO_SOCKET_FDS:-0}" = '1' ]; then
    for fd_path in /proc/$$/fd/*; do
        case "$(readlink "$fd_path" 2>/dev/null || true)" in
            socket:*)
                echo "inherited socket fd: $fd_path" >&2
                exit 1
                ;;
        esac
    done
fi

if [ -n "${MOCK_ADB_KEEPALIVE_PID_FILE:-}" ] &&
   [ ! -s "$MOCK_ADB_KEEPALIVE_PID_FILE" ]; then
    sleep 30 </dev/null >/dev/null 2>&1 &
    printf '%s\n' "$!" >"$MOCK_ADB_KEEPALIVE_PID_FILE"
fi

[ "$1" = "-s" ]
serial="$2"
[ "$3" = "shell" ]
[ "$4" = "cat" ]
[ "$5" = "/sys/devices/virtual/power/zte_power/adc2_temp" ]

case "$serial" in
    V3E1T12345) printf '47\r\n' ;;
    V3E2T12345) printf '46\r\n' ;;
    *) exit 1 ;;
esac
