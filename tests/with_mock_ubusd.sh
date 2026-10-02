#!/bin/sh
# Run one integration suite with every datad ubus call going over the socket
# protocol (strict mode: no CLI fallback) to tests/mock_ubusd.py.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
dir=$(mktemp -d)
python3 "$here/mock_ubusd.py" "$dir/ubus.sock" "$dir/invokes" &
server=$!
trap 'kill "$server" 2>/dev/null || true; rm -rf "$dir"' EXIT INT TERM
for _ in $(seq 50); do [ -S "$dir/ubus.sock" ] && break; sleep 0.1; done
export ZWRT_DATAD_UBUS=socket ZWRT_DATAD_UBUS_SOCKET="$dir/ubus.sock"
"$@"
count=$(cat "$dir/invokes")
[ "$count" -gt 0 ] || { echo "no ubus call reached the socket" >&2; exit 1; }
echo "socket ubus: $count calls over the protocol"
