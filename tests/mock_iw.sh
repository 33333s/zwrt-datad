#!/bin/sh
set -eu
[ -z "${MOCK_CALL_LOG:-}" ] || printf 'iw\t%s\n' "$*" >>"$MOCK_CALL_LOG"
case "$*" in
    'dev wlan0 get power_save'|'dev wlan1 get power_save') printf '%s\n' 'Power save: off' ;;
    'dev')
        cat <<'EOF'
phy#0
 Interface wlan1
  ifindex 11
  ssid Fixture 2G
  type AP
  channel 1 (2412 MHz)
  txpower 19.00 dBm
phy#1
 Interface wlan0
  ifindex 12
  ssid Fixture 5G
  type AP
  channel 36 (5180 MHz)
  txpower 18.00 dBm
EOF
        ;;
esac
