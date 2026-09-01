#!/bin/sh
set -eu

form=''
url=''
while [ "$#" -gt 0 ]; do
    case "$1" in
        --data)
            shift
            form="${1:-}"
            ;;
        http://*)
            url="$1"
            ;;
    esac
    shift
done
[ -n "${MOCK_CURL_LOG:-}" ] && printf '%s\t%s\n' "$url" "$form" >>"$MOCK_CURL_LOG"
printf '%s\n' '{"result":"success"}'
