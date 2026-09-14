#!/bin/sh
set -eu
[ -z "${MOCK_CALL_LOG:-}" ] || printf 'hostapd_cli\t%s\n' "$*" >>"$MOCK_CALL_LOG"
printf 'state=ENABLED\n'
