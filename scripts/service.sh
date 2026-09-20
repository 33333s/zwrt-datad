#!/system/bin/sh

SERVICE_DIR="${ZWRT_DATAD_DIR:-/data/zwrt-datad}"
BIN="${ZWRT_DATAD_BIN:-$SERVICE_DIR/zwrt-datad}"
PID_FILE="${ZWRT_DATAD_PID_FILE:-$SERVICE_DIR/zwrt-datad.pid}"
LOG_FILE="${ZWRT_DATAD_LOG_FILE:-$SERVICE_DIR/zwrt-datad.log}"
TOKEN_FILE="${ZWRT_DATAD_TOKEN_FILE:-$SERVICE_DIR/auth.token}"

binary_real_path() {
    path="$(readlink -f "$BIN" 2>/dev/null)"
    [ -n "$path" ] && printf '%s\n' "$path" || printf '%s\n' "$BIN"
}

process_matches() {
    check_pid="$1"
    case "$check_pid" in
        ''|*[!0-9]*) return 1 ;;
    esac
    [ -d "/proc/$check_pid" ] || return 1

    expected="$(binary_real_path)"
    actual="$(readlink "/proc/$check_pid/exe" 2>/dev/null)"
    actual="${actual% (deleted)}"
    if [ -n "$actual" ] && { [ "$actual" = "$expected" ] || [ "$actual" = "$BIN" ]; }; then
        return 0
    fi
    first_arg="$(tr '\000' '\n' 2>/dev/null < "/proc/$check_pid/cmdline" | sed -n '1p')"
    [ "$first_arg" = "$expected" ] || [ "$first_arg" = "$BIN" ]
}

write_pid() {
    tmp_pid_file="$PID_FILE.$$"
    umask 022
    printf '%s\n' "$1" > "$tmp_pid_file" && mv -f "$tmp_pid_file" "$PID_FILE"
}

find_running_pid() {
    for proc_dir in /proc/[0-9]*; do
        scan_pid="${proc_dir#/proc/}"
        if process_matches "$scan_pid"; then
            printf '%s\n' "$scan_pid"
            return 0
        fi
    done
    return 1
}

running_pid() {
    if [ -f "$PID_FILE" ]; then
        IFS= read -r saved_pid < "$PID_FILE"
        if process_matches "$saved_pid"; then
            printf '%s\n' "$saved_pid"
            return 0
        fi
        rm -f "$PID_FILE"
    fi
    found_pid="$(find_running_pid)"
    if [ -n "$found_pid" ]; then
        write_pid "$found_pid" >/dev/null 2>&1 || true
        printf '%s\n' "$found_pid"
        return 0
    fi
    return 1
}

stop_legacy_cloud() {
    [ ! -f "$SERVICE_DIR/cloud-service.sh" ] || sh "$SERVICE_DIR/cloud-service.sh" stop >/dev/null 2>&1 || true
    rm -f "$SERVICE_DIR/cloud.pid" "$SERVICE_DIR/cloud.sock" "$SERVICE_DIR/cloud.lock"
}

ensure_auth_token() {
    if [ -e "$TOKEN_FILE" ] || [ -L "$TOKEN_FILE" ]; then
        if [ ! -f "$TOKEN_FILE" ] || [ -L "$TOKEN_FILE" ] || [ ! -s "$TOKEN_FILE" ]; then
            echo "zwrt-datad 启动失败：$TOKEN_FILE 必须是非空普通文件" >&2
            return 1
        fi
        chmod 600 "$TOKEN_FILE" 2>/dev/null || return 1
        return 0
    fi

    token_dir="${TOKEN_FILE%/*}"
    [ "$token_dir" != "$TOKEN_FILE" ] || token_dir=.
    mkdir -p "$token_dir" || return 1
    token_tmp="$TOKEN_FILE.new.$$"
    umask 077
    (set -C; : > "$token_tmp") 2>/dev/null || {
        echo "zwrt-datad 启动失败：无法安全创建 Token 临时文件" >&2
        return 1
    }
    generated_token="$(od -An -N32 -tx1 /dev/urandom | tr -d ' \n')"
    case "$generated_token" in
        *[!0-9a-f]*|'') rm -f "$token_tmp"; return 1 ;;
    esac
    if [ "${#generated_token}" -ne 64 ] ||
        ! printf '%s\n' "$generated_token" > "$token_tmp"; then
        rm -f "$token_tmp"
        return 1
    fi
    chmod 600 "$token_tmp" && mv -f "$token_tmp" "$TOKEN_FILE"
}

discover_cooling_paths() {
    if [ -z "${ZWRT_DATAD_COOLING_ZONE_PATH:-}" ]; then
        for type_file in /sys/class/thermal/thermal_zone*/type; do
            [ -f "$type_file" ] || continue
            [ "$(cat "$type_file" 2>/dev/null)" = "sys-therm-4" ] || continue
            ZWRT_DATAD_COOLING_ZONE_PATH="${type_file%/type}"
            export ZWRT_DATAD_COOLING_ZONE_PATH
            break
        done
    fi
    if [ -z "${ZWRT_DATAD_FAN_COOLING_STATE_PATH:-}" ]; then
        for type_file in /sys/class/thermal/cooling_device*/type; do
            [ -f "$type_file" ] || continue
            [ "$(cat "$type_file" 2>/dev/null)" = "pwm-fan" ] || continue
            ZWRT_DATAD_FAN_COOLING_STATE_PATH="${type_file%/type}/cur_state"
            export ZWRT_DATAD_FAN_COOLING_STATE_PATH
            break
        done
    fi
}

start() {
    active_pid="$(running_pid)"
    if [ -n "$active_pid" ]; then
        stop_legacy_cloud
        echo "zwrt-datad 正在运行 (PID $active_pid)"
        return 0
    fi
    if [ ! -f "$BIN" ]; then
        echo "zwrt-datad 启动失败：找不到 $BIN" >&2
        return 1
    fi
    chmod 755 "$BIN" 2>/dev/null || true
    if [ ! -x "$BIN" ]; then
        echo "zwrt-datad 启动失败：$BIN 不可执行" >&2
        return 1
    fi

    mkdir -p "$SERVICE_DIR" || return 1
    cd "$SERVICE_DIR" || return 1
    stop_legacy_cloud
    ensure_auth_token || return 1
    discover_cooling_paths
    nohup "$BIN" -i 1000 -b 127.0.0.1 -p 9460 --webshell \
        --lan-bind 0.0.0.0 --lan-port 9461 --auth-token-file "$TOKEN_FILE" \
        >> "$LOG_FILE" 2>&1 </dev/null &
    launched_pid=$!
    sleep 1
    if process_matches "$launched_pid"; then
        write_pid "$launched_pid" || true
        stop_legacy_cloud
        echo "zwrt-datad 已启动 (PID $launched_pid)"
        return 0
    fi
    active_pid="$(find_running_pid)"
    if [ -n "$active_pid" ]; then
        write_pid "$active_pid" || true
        stop_legacy_cloud
        echo "zwrt-datad 已启动 (PID $active_pid)"
        return 0
    fi
    rm -f "$PID_FILE"
    echo "zwrt-datad 启动失败，请检查 $LOG_FILE" >&2
    return 1
}

stop() {
    stop_legacy_cloud
    active_pid="$(running_pid)"
    if [ -z "$active_pid" ]; then
        rm -f "$PID_FILE"
        echo "zwrt-datad 未在运行"
        return 0
    fi
    kill -TERM "$active_pid" 2>/dev/null || true
    wait_count=0
    while process_matches "$active_pid" && [ "$wait_count" -lt 10 ]; do
        sleep 1
        wait_count=$((wait_count + 1))
    done
    if process_matches "$active_pid"; then
        kill -KILL "$active_pid" 2>/dev/null || true
    fi
    rm -f "$PID_FILE"
    echo "zwrt-datad 已停止"
}

status() {
    active_pid="$(running_pid)"
    if [ -n "$active_pid" ]; then
        echo "zwrt-datad 正在运行 (PID $active_pid)"
        return 0
    fi
    echo "zwrt-datad 未在运行"
    return 1
}

case "$1" in
    start) start ;;
    stop) stop ;;
    restart) stop && start ;;
    status) status ;;
    *)
        echo "用法: $0 {start|stop|restart|status}"
        exit 1
        ;;
esac
