#!/bin/sh
# Runs under nohup on the modem. ADB is only a transport/observer, never the
# transaction owner. Backups live on /cache; no partition/firmware operations.
set -eu
umask 077
DIR=/cache/zwrt-datad
STAGE=${1:?missing staging directory}
case "$STAGE" in "$DIR"/.deploy.*) ;; *) exit 2 ;; esac
[ "$(dirname "$STAGE")" = "$DIR" ] && [ ! -L "$STAGE" ] || exit 2
[ "$(cd "$STAGE" && pwd -P)" = "$STAGE" ] || exit 2
LOCK=$DIR/.deploy-lock
UNIT_PATH=/etc/systemd/system/zwrt-datad.service
WANTS=/etc/systemd/system/multi-user.target.wants/zwrt-datad.service
MOUNTS=/proc/mounts
UBI_RO=/sys/class/ubi/ubi0/ro_mode
committed=0
replacing=0
stopping=0
unit_changed=0
root_restore=0
old_binary=0
had_unit=0
had_link=0
mode=session

result() { printf '%s\n' "$*" > "$STAGE/result.tmp"; mv "$STAGE/result.tmp" "$STAGE/result"; }
if ! mkdir "$LOCK"; then result 'FAILED: another deployment holds the lock'; exit 1; fi
printf '%s\n' "$$" > "$LOCK/pid"

restore_root() {
    [ "$root_restore" = 1 ] || return 0
    opts=$(awk '$2 == "/" {print $4; exit}' "$MOUNTS")
    case ",$opts," in *,ro,*) root_restore=0; return 0 ;; esac
    sync
    if mount -o remount,ro /; then
        opts=$(awk '$2 == "/" {print $4; exit}' "$MOUNTS")
        case ",$opts," in *,ro,*) root_restore=0; return 0 ;; esac
    fi
    echo 'ERROR: unable to restore read-only root mount' >&2
    return 1
}

open_root() {
    opts=$(awk '$2 == "/" {print $4; exit}' "$MOUNTS")
    case ",$opts," in
        *,rw,*) return 0 ;;
        *,ro,*)
            # Do not attempt writes/remounts on a read-only UBI attachment.
            [ "$(cat "$UBI_RO" 2>/dev/null)" = 0 ] || return 1
            root_restore=1
            mount -o remount,rw / || return 1
            opts=$(awk '$2 == "/" {print $4; exit}' "$MOUNTS")
            case ",$opts," in *,rw,*) return 0 ;; esac ;;
    esac
    return 1
}

restore_unit() {
    [ "$unit_changed" = 1 ] || return 0
    open_root || return 1
    if [ "$had_unit" = 1 ]; then
        cp -p "$STAGE/old.unit" "$UNIT_PATH.deploy-new" && mv -f "$UNIT_PATH.deploy-new" "$UNIT_PATH" || return 1
    else rm -f "$UNIT_PATH" || return 1; fi
    if [ "$had_link" = 1 ]; then
        ln -sfn "$(cat "$STAGE/old.link")" "$WANTS" || return 1
    else rm -f "$WANTS" || return 1; fi
    ctl daemon-reload || return 1
    restore_root || return 1
    unit_changed=0
}

finish() {
    rc=$?
    trap - EXIT HUP INT TERM
    set +e
    recovery=ok
    if [ "$committed" != 1 ]; then
        echo 'Deployment failed; checking/restoring previous installation...'
        if [ "$replacing" = 1 ]; then
            if stop_service; then
                for file in zwrt-datad start.sh service-control.sh zwrt-datad.service; do
                    if [ -f "$STAGE/old.$file" ]; then
                        cp -p "$STAGE/old.$file" "$DIR/$file.restore" && mv -f "$DIR/$file.restore" "$DIR/$file" || recovery=failed
                    else rm -f "$DIR/$file" || recovery=failed; fi
                done
            else recovery=failed; fi
        fi
        restore_unit || recovery=failed
        if [ "$stopping" = 1 ] && [ "$old_binary" = 1 ] && [ "$recovery" = ok ]; then
            start_service && wait_healthy "$old_sha" || recovery=failed
        fi
        restore_root || recovery=failed
        if [ "$recovery" = ok ]; then result 'FAILED: previous installation restored (or untouched)'
        else result "FAILED: recovery requires attention; backups retained in $STAGE"; fi
        rc=1
    elif ! restore_root; then
        result "FAILED: root mount restoration requires attention; backups in $STAGE"
        rc=1
    else
        result "SUCCESS: $mode; executable hash + socket owner stable for $STABLE_SECONDS seconds"
    fi
    # Never remove a computed directory tree. Keep logs/results, and keep all
    # backups on failure. Successful upgrades retain one previous binary.
    if [ "$rc" = 0 ]; then
        rm -f "$STAGE/zwrt-datad" "$STAGE/old.zwrt-datad" "$STAGE/old.start.sh" \
            "$STAGE/old.service-control.sh" "$STAGE/old.zwrt-datad.service" "$STAGE/old.unit" "$STAGE/old.link"
    fi
    rm -f "$LOCK/pid"
    rmdir "$LOCK" || true
    exit "$rc"
}
trap finish EXIT
trap 'exit 1' HUP INT TERM

cd "$STAGE"
echo 'Verifying staged files and existing installation...'
sha256sum -c SHA256SUMS
. "$STAGE/service-control.sh"
new_sha=$(binary_sha "$STAGE/zwrt-datad")
version=$(timeout 10 "$STAGE/zwrt-datad" --version)
case "$version" in 'zwrt-datad '*) ;; *) echo 'unexpected version response' >&2; exit 1 ;; esac
check_unit_owner
for file in zwrt-datad start.sh service-control.sh zwrt-datad.service; do
    [ ! -L "$DIR/$file" ] || { echo "refusing symlink: $file" >&2; exit 1; }
    if [ -e "$DIR/$file" ]; then cp -p "$DIR/$file" "$STAGE/old.$file"; fi
done
if [ -f "$STAGE/old.zwrt-datad" ]; then old_binary=1; old_sha=$(binary_sha "$STAGE/old.zwrt-datad"); fi
# Refuse unrelated listeners before stopping or replacing anything.
if port_busy && { [ "$old_binary" != 1 ] || ! healthy_token "$old_sha" >/dev/null; }; then
    echo 'port 9460 is not owned by the installed binary' >&2; exit 1
fi
[ ! -L "$UNIT_PATH" ] || { echo 'refusing symlinked persistent unit' >&2; exit 1; }
if [ -f "$UNIT_PATH" ]; then had_unit=1; cp -p "$UNIT_PATH" "$STAGE/old.unit"; fi
if [ -L "$WANTS" ]; then had_link=1; readlink "$WANTS" > "$STAGE/old.link"
elif [ -e "$WANTS" ]; then echo 'unexpected regular file at wants link' >&2; exit 1; fi

stopping=1
echo 'Stopping the owned datad service...'
stop_service
replacing=1
echo 'Installing verified files (previous files backed up)...'
for file in zwrt-datad start.sh service-control.sh zwrt-datad.service; do
    # Keep the verified staging copy until health is confirmed. Rename on the
    # same volume is atomic; no live executable is truncated in place.
    cp "$STAGE/$file" "$DIR/$file.deploy-new"
    chmod 700 "$DIR/$file.deploy-new"
    mv -f "$DIR/$file.deploy-new" "$DIR/$file"
done

echo 'Configuring startup and restoring original root mount mode...'
if open_root; then
    unit_changed=1
    cp "$DIR/zwrt-datad.service" "$UNIT_PATH.deploy-new"
    chmod 644 "$UNIT_PATH.deploy-new"
    mv -f "$UNIT_PATH.deploy-new" "$UNIT_PATH"
    mkdir -p "$(dirname "$WANTS")"
    ln -sfn ../zwrt-datad.service "$WANTS"
    cmp -s "$DIR/zwrt-datad.service" "$UNIT_PATH"
    [ "$(readlink "$WANTS")" = ../zwrt-datad.service ]
    ctl daemon-reload
    restore_root
    mode='persistent boot autostart'
else
    # A read-only UBI attachment is a supported session-only installation.
    # If a remount was attempted, verify its restoration before continuing.
    restore_root
    if [ "$had_unit" = 1 ] && [ "$had_link" = 1 ]; then mode='existing persistent unit'; fi
fi
echo 'Starting datad...'
start_service
echo "Checking executable and socket ownership; must stay healthy for $STABLE_SECONDS seconds..."
wait_healthy "$new_sha"
if [ "$old_binary" = 1 ]; then
    cp -p "$STAGE/old.zwrt-datad" "$DIR/zwrt-datad.prev.new"
    mv -f "$DIR/zwrt-datad.prev.new" "$DIR/zwrt-datad.prev"
fi
sync
committed=1
