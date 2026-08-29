#!/bin/sh
set -eu

if [ -n "${MOCK_ADB_CALL_LOG:-}" ]; then
    printf 'adb\t%s\n' "$*" >>"$MOCK_ADB_CALL_LOG"
fi

[ "$1" = "-s" ]
serial="$2"
[ "$3" = "shell" ]
case "$4" in
    cat)
        [ "$5" = "/sys/devices/virtual/power/zte_power/adc2_temp" ]
        case "$serial" in
            V3E1T12345) printf '47\r\n' ;;
            V3E2T12345) printf '46\r\n' ;;
            *) exit 1 ;;
        esac
        ;;
    'grep QCI= /logfs/key.log | tail -n 64')
        case "$serial" in
            V3E1T12345)
                printf '%s\r\n' \
                    '2026-08-26 11:26:49 : [DATA] cid1, QCI=[9], DL_AMBR=[100000]kbps, UL_AMBR=[100000]kbps'
                ;;
            V3E2T12345)
                printf '%s\r\n' \
                    '2026-08-26 11:52:03 : [DATA] cid1, QCI=[9], DL_AMBR=[150000]kbps, UL_AMBR=[75000]kbps'
                ;;
            *) exit 1 ;;
        esac
        ;;
    *) exit 1 ;;
esac
