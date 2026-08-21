#!/bin/sh
set -eu

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
