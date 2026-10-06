#!/usr/bin/env bash
# U50 Pro (MU5120), ARMv7. Only /cache and the datad systemd unit are changed.
# The on-device transaction survives adb disconnects and restores the previous
# installation if the new process fails verification. Never enables factory
# diag, writes calibration partitions, or reboots the modem.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN=""
SERIAL="${U50_ADB_SERIAL:-}"
while [ $# -gt 0 ]; do
    case "$1" in
        --serial) SERIAL="${2:?missing serial}"; shift 2 ;;
        --serial=*) SERIAL="${1#*=}"; shift ;;
        *) if [ -z "$BIN" ]; then BIN="$1"; shift; else echo "unexpected argument: $1" >&2; exit 2; fi ;;
    esac
done
BIN="${BIN:-$ROOT/build/zwrt-datad-armv7-candidate}"
DIR=/cache/zwrt-datad
export MSYS_NO_PATHCONV=1 MSYS2_ARG_CONV_EXCL='*'
win_path() { if command -v cygpath >/dev/null 2>&1; then cygpath -m "$1"; else printf '%s' "$1"; fi; }
die() { echo "deploy-u50pro: $*" >&2; exit 1; }
[ -f "$BIN" ] || die "binary not found: $BIN"
[ "$(od -An -tx1 -N6 "$BIN" | tr -d ' \n\r')" = 7f454c460101 ] || die "expected little-endian ELF32 ARM"
[ "$(od -An -tx1 -j18 -N2 "$BIN" | tr -d ' \n\r')" = 2800 ] || die "expected ARM e_machine (not x86/AArch64)"
command -v adb >/dev/null 2>&1 || die "adb not found in PATH"
ADB=(adb)
[ -z "$SERIAL" ] || ADB+=(-s "$SERIAL")
"${ADB[@]}" get-state 2>/dev/null | tr -d '\r' | grep -qx device || die "select one connected modem with --serial"
"${ADB[@]}" shell 'test "$(id -u)" = 0 && test "$(uname -m)" = armv7l && test "$(cfg get model_name)" = MU5120' || die "expected root adb on MU5120 / armv7l"
sha_host="$(sha256sum "$BIN" | cut -d' ' -f1)"
bytes="$(wc -c < "$BIN" | tr -d ' ')"
required_kb=$(( (bytes * 3 + 4194304 + 1023) / 1024 ))
"${ADB[@]}" shell "test ! -L $DIR && umask 077 && mkdir -p $DIR && test ! -e $DIR/.deploy-lock && free=\$(df -Pk $DIR | tail -1 | awk '{print \$4}') && test \"\$free\" -ge $required_kb" || die "deployment locked, unsafe data path, or insufficient /cache space"
mkdir -p "$ROOT/build"
stage_dir="$(mktemp -d "$ROOT/build/.deploy-u50pro.XXXXXX")"
cleanup() {
    # Delete only our known local staging files, never a computed directory tree.
    case "$stage_dir" in "$ROOT/build/.deploy-u50pro."*)
        rm -f "$stage_dir/start.sh" "$stage_dir/zwrt-datad.service" "$stage_dir/SHA256SUMS"
        rmdir "$stage_dir" 2>/dev/null || true ;;
    esac
}
trap cleanup EXIT
cat > "$stage_dir/start.sh" <<'SH'
#!/bin/sh
. /cache/zwrt-datad/service-control.sh
start_service
SH
cat > "$stage_dir/zwrt-datad.service" <<'UNIT'
[Unit]
Description=zwrt-datad (ZWRT backend daemon, U50 Pro)
RequiresMountsFor=/cache
After=multi-user.target

[Service]
ExecStart=/cache/zwrt-datad/zwrt-datad --u50-model u50pro --u50-data-dir /cache/zwrt-datad --u50-signaling --bind 127.0.0.1 --port 9460
Restart=on-failure
RestartSec=3

[Install]
WantedBy=multi-user.target
UNIT
remote_stage="$("${ADB[@]}" shell "umask 077; mktemp -d $DIR/.deploy.XXXXXX" | tr -d '\r')"
[[ "$remote_stage" =~ ^/cache/zwrt-datad/\.deploy\.[a-zA-Z0-9]+$ ]] || die "invalid remote staging directory"
echo "== stage into $remote_stage (running service unchanged) =="
: > "$stage_dir/SHA256SUMS"
for name in zwrt-datad start.sh zwrt-datad.service service-control.sh deploy-transaction.sh; do
    case "$name" in
        zwrt-datad) input="$BIN" ;;
        service-control.sh) input="$ROOT/scripts/u50pro-service.sh" ;;
        deploy-transaction.sh) input="$ROOT/scripts/u50pro-deploy-transaction.sh" ;;
        *) input="$stage_dir/$name" ;;
    esac
    printf '%s  %s\n' "$(sha256sum "$input" | cut -d' ' -f1)" "$name" >> "$stage_dir/SHA256SUMS"
    "${ADB[@]}" push "$(win_path "$input")" "$remote_stage/$name" >/dev/null
done
"${ADB[@]}" push "$(win_path "$stage_dir/SHA256SUMS")" "$remote_stage/SHA256SUMS" >/dev/null
"${ADB[@]}" shell "cd $remote_stage && sha256sum -c SHA256SUMS && chmod 700 zwrt-datad && timeout 10 ./zwrt-datad --version" || die "staging verification failed; installed service was not changed"
echo "== starting independent device transaction =="
"${ADB[@]}" shell "nohup sh $remote_stage/deploy-transaction.sh $remote_stage > $remote_stage/deploy.log 2>&1 < /dev/null &" || die "could not launch transaction; inspect $remote_stage"
for ((i=0; i<90; i++)); do
    result="$("${ADB[@]}" shell "cat $remote_stage/result 2>/dev/null" 2>/dev/null | tr -d '\r' || true)"
    case "$result" in
        SUCCESS*)
            echo "$result"
            echo "binary: $DIR/zwrt-datad (sha256 $sha_host)"
            echo "previous binary: $DIR/zwrt-datad.prev (when upgrading)"
            echo "API: adb forward tcp:9460 tcp:9460"
            exit 0 ;;
        FAILED*)
            "${ADB[@]}" shell "cat $remote_stage/deploy.log" || true
            die "$result" ;;
    esac
    sleep 2
done
die "transaction status unavailable; device keeps managing rollback. Read $remote_stage/result and deploy.log before retrying"
