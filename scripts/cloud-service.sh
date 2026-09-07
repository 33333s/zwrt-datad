#!/system/bin/sh
set -eu
DIR="${ZWRT_DATAD_DIR:-/data/zwrt-datad}"
BIN="$DIR/zwrt-datad-cloud"
PID="$DIR/cloud.pid"
match() {
 case "${1:-}" in ''|*[!0-9]*) return 1;; esac
 actual="$(readlink "/proc/$1/exe" 2>/dev/null || true)"
 [ "${actual% (deleted)}" = "$BIN" ]
}
start() {
 [ -x "$BIN" ] || return 0
 if [ -f "$PID" ] && match "$(cat "$PID")"; then return 0; fi
 mkdir -p "$DIR"
 umask 077
 nohup "$BIN" --dir "$DIR" >/dev/null 2>&1 </dev/null &
 printf '%s\n' "$!" > "$PID"
}
stop() {
 if [ -f "$PID" ]; then
  pid="$(cat "$PID")"
  if match "$pid"; then
   kill -TERM "$pid"
   n=0
   while match "$pid" && [ "$n" -lt 15 ]; do sleep 1; n=$((n+1)); done
   if match "$pid"; then kill -KILL "$pid"; fi
  fi
  rm -f "$PID"
 fi
}
case "${1:-}" in start) start;; stop) stop;; restart) stop; start;; status) [ -f "$PID" ] && match "$(cat "$PID")";; *) exit 1;; esac
