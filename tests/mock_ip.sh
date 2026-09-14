#!/bin/sh
set -eu
[ "$*" = 'neigh show dev br-lan' ] || exit 1
cat <<'EOF'
192.168.0.2 dev br-lan lladdr 00:11:22:33:44:55 REACHABLE
192.168.0.7 dev br-lan lladdr 00:11:22:33:44:77 REACHABLE
192.168.0.8 dev br-lan lladdr 00:11:22:33:44:88 REACHABLE
EOF
