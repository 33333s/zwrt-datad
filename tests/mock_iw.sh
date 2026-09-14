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
        if [ -n "${MOCK_NET_CLASS_DIR:-}" ]; then
            for extra in wlan4 wlan5; do
                [ -d "$MOCK_NET_CLASS_DIR/$extra" ] || continue
                printf ' Interface %s\n  ifindex %s\n  ssid Fixture Extra\n  type AP\n  channel 36 (5180 MHz)\n  txpower 18.00 dBm\n' \
                    "$extra" "$(cat "$MOCK_NET_CLASS_DIR/$extra/ifindex")"
            done
        fi
        ;;
    'dev wlan0 interface add wlan4 type __ap'|'dev wlan1 interface add wlan4 type __ap')
        mkdir -p "$MOCK_NET_CLASS_DIR/wlan4"
        printf '104\n' >"$MOCK_NET_CLASS_DIR/wlan4/ifindex"
        ;;
    'dev wlan0 interface add wlan5 type __ap'|'dev wlan1 interface add wlan5 type __ap')
        mkdir -p "$MOCK_NET_CLASS_DIR/wlan5"
        printf '105\n' >"$MOCK_NET_CLASS_DIR/wlan5/ifindex"
        ;;
    'dev wlan4 del') rm -rf "$MOCK_NET_CLASS_DIR/wlan4" ;;
    'dev wlan5 del') rm -rf "$MOCK_NET_CLASS_DIR/wlan5" ;;
esac
