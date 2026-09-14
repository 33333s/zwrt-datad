#!/bin/sh
set -eu
[ "$*" = 'fdb show br br-lan' ] || exit 1
cat <<'EOF'
00:11:22:33:44:55 dev wlan0 master br-lan
00:11:22:33:44:77 dev wlan0 master br-lan
00:11:22:33:44:88 dev eth1 master br-lan
00:11:22:33:44:99 dev eth1 master br-lan
02:00:00:00:00:01 dev eth1 master br-lan permanent
33:33:00:00:00:01 dev br-lan self permanent
EOF
