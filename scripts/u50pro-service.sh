#!/bin/sh
# Shared by start.sh and the detached deployment transaction. No modem writes.
DIR=/cache/zwrt-datad
PROC=/proc
UNIT=zwrt-datad.service
PORT_HEX=24E4
WAIT_SECONDS=30
STABLE_SECONDS=10

binary_sha() { sha256sum "$1" 2>/dev/null | awk '{print $1}'; }

owned_pid() {
    case "$1" in ''|*[!0-9]*|0|1) return 1 ;; esac
    exe=$(readlink "$PROC/$1/exe" 2>/dev/null) || return 1
    case "$exe" in "$DIR/zwrt-datad"|"$DIR/zwrt-datad (deleted)") return 0 ;; esac
    return 1
}

owned_pids() {
    for entry in "$PROC"/[0-9]*; do
        pid=${entry##*/}
        if owned_pid "$pid"; then printf '%s\n' "$pid"; fi
    done
}

port_busy() {
    for table in "$PROC/net/tcp" "$PROC/net/tcp6"; do
        [ -r "$table" ] || continue
        if awk -v port=":$PORT_HEX" '$2 ~ (port "$") && $4 == "0A" {found=1} END {exit !found}' "$table"; then return 0; fi
    done
    return 1
}

# The socket must belong to this exact executable, not merely occupy its port.
healthy_token() {
    expected=$1
    for hp in $(owned_pids); do
        [ "$(binary_sha "$PROC/$hp/exe")" = "$expected" ] || continue
        inodes=$(awk -v addr="0100007F:$PORT_HEX" '$2 == addr && $4 == "0A" {print $10}' "$PROC/net/tcp")
        for inode in $inodes; do
            for fd in "$PROC/$hp/fd"/*; do
                [ "$(readlink "$fd" 2>/dev/null)" = "socket:[$inode]" ] || continue
                # PID plus kernel start time also detects rapid PID reuse.
                started=$(awk '{print $22}' "$PROC/$hp/stat" 2>/dev/null)
                [ -n "$started" ] || continue
                printf '%s:%s\n' "$hp" "$started"
                return 0
            done
        done
    done
    return 1
}

wait_healthy() {
    want=$1
    previous= consecutive=0 elapsed=0
    while [ "$elapsed" -lt "$WAIT_SECONDS" ]; do
        current=$(healthy_token "$want") || current=
        if [ -n "$current" ] && [ "$current" = "$previous" ]; then
            consecutive=$((consecutive + 1))
            [ "$consecutive" -ge "$STABLE_SECONDS" ] && return 0
        else
            consecutive=0
        fi
        previous=$current
        elapsed=$((elapsed + 1))
        sleep 1
    done
    return 1
}

has_unit() { command -v systemctl >/dev/null 2>&1 && systemctl --no-pager cat "$UNIT" >/dev/null 2>&1; }

check_unit_owner() {
    if has_unit; then
        systemctl --no-pager cat "$UNIT" 2>/dev/null | grep -Eq "^ExecStart=$DIR/zwrt-datad([[:space:]]|$)" || {
            echo "refusing to operate on an unrelated $UNIT" >&2; return 1;
        }
        main=$(systemctl --no-pager show "$UNIT" -p MainPID --value 2>/dev/null)
        case "$main" in ''|0) ;; *) owned_pid "$main" || return 1 ;; esac
    fi
}

stop_service() {
    check_unit_owner || return 1
    if has_unit; then systemctl --no-block stop "$UNIT" || return 1; fi
    for sp in $(owned_pids); do
        # Never pkill by name: another daemon/DIAG session must remain untouched.
        owned_pid "$sp" && kill -TERM "$sp" 2>/dev/null || true
    done
    elapsed=0
    while [ "$elapsed" -lt "$WAIT_SECONDS" ]; do
        state=$(systemctl --no-pager show "$UNIT" -p ActiveState --value 2>/dev/null || true)
        if [ -z "$(owned_pids)" ]; then
            case "$state" in active|activating|deactivating) ;; *) return 0 ;; esac
        fi
        sleep 1
        elapsed=$((elapsed + 1))
    done
    echo "old service did not stop; refusing to force-kill it" >&2
    return 1
}

start_service() {
    wanted=$(binary_sha "$DIR/zwrt-datad")
    [ -n "$wanted" ] || return 1
    if healthy_token "$wanted" >/dev/null; then return 0; fi
    if port_busy; then echo "port 9460 is occupied by an unverified process" >&2; return 1; fi
    check_unit_owner || return 1
    if has_unit; then
        systemctl reset-failed "$UNIT" 2>/dev/null || true
        systemctl start "$UNIT"
    elif command -v systemd-run >/dev/null 2>&1; then
        systemctl reset-failed "$UNIT" 2>/dev/null || true
        systemd-run --unit="$UNIT" --description='zwrt-datad ZWRT backend' \
            --property=Restart=on-failure --property=RestartSec=3 \
            "$DIR/zwrt-datad" --u50-model u50pro --u50-data-dir "$DIR" \
            --u50-signaling --bind 127.0.0.1 --port 9460
    else
        nohup "$DIR/zwrt-datad" --u50-model u50pro --u50-data-dir "$DIR" \
            --u50-signaling --bind 127.0.0.1 --port 9460 >> "$DIR/zwrt-datad.log" 2>&1 < /dev/null &
    fi
}
