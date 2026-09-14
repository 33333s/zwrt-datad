#!/bin/sh
set -eu

if [ "$#" = 1 ] && [ "$1" = dev ]; then
    printf '%s\n' 'phy#0' '    Interface wlan0' '        type AP'
    exit 0
fi
if [ "$#" = 4 ] && [ "$1" = dev ] && [ "$2" = wlan0 ] &&
   [ "$3" = station ] && [ "$4" = dump ]; then
    cat <<'EOF'
Station 00:11:22:33:44:55 (on wlan0)
	authorized:	yes
Station 00:11:22:33:44:77 (on wlan0)
	authorized:	yes
EOF
    exit 0
fi
exit 1
