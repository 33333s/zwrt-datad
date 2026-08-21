#!/bin/sh
set -eu

if [ -n "${MOCK_CALL_LOG:-}" ]; then
    printf 'uci\t%s\n' "$*" >>"$MOCK_CALL_LOG"
fi

case "$*" in
    *__mock_fail__*) exit 1 ;;
esac

if [ "${1:-}" = "-q" ] && [ "${2:-}" = "get" ]; then
    case "${3:-}" in
        wireless.main_2g.ssid) printf '%s\n' 'Fixture 2G' ;;
        wireless.main_2g.key) printf '%s\n' 'fixture-password' ;;
        wireless.main_2g.encryption) printf '%s\n' 'sae-mixed' ;;
        wireless.main_2g.disabled) printf '%s\n' '0' ;;
        wireless.main_5g.ssid) printf '%s\n' 'Fixture 5G' ;;
        wireless.main_5g.key) printf '%s\n' 'fixture-password' ;;
        wireless.main_5g.encryption) printf '%s\n' 'sae-mixed' ;;
        wireless.main_5g.disabled) printf '%s\n' '0' ;;
        *) exit 1 ;;
    esac
    exit 0
fi

case "${1:-}" in
    set|commit|revert|add_list|del_list) exit 0 ;;
esac

exit 1
