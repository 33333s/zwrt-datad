#!/bin/sh
set -eu

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

exit 0
