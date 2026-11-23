# Sourced by adapter.sh.
# check-shell-paths: no-errexit the adapter suite records failed checks and continues.

vnc_sock() {
    # vmlab keeps a VM's sockets in <runtime>/labs/<lab>/vms/<vm>/. The runtime
    # directory is $XDG_RUNTIME_DIR/vmlab, or /tmp/vmlab-<uid>-<hash> when that
    # is too long for a socket path, and a long lab or VM name is hashed too
    # (src/paths.rs in the vmlab repository). The lab daemon's command line has
    # the name the lab runs under.
    local xdg=${XDG_RUNTIME_DIR:-/run/user/$(id -u)} lab base l v
    lab=$(lab_name)
    [ -n "$lab" ] || return 1
    h() { printf %s "$1" | sha256sum | cut -c1-12; }
    for base in "$xdg/vmlab" "/tmp/vmlab-$(id -u)-$(h "$xdg")"; do
        for l in "$lab" "_$(h "$lab")"; do
            for v in "$VM" "_$(h "$VM")"; do
                [ -S "$base/labs/$l/vms/$v/vnc.sock" ] && { echo "$base/labs/$l/vms/$v/vnc.sock"; return 0; }
            done
        done
    done
    return 1
}

check_tear() { # check_tear flip|blt
    local mode=$1 name=tear-$1 sock gpid
    note "$name"
    sock=$(vnc_sock)
    [ -n "$sock" ] || { skip "$name" "found no vnc.sock for this lab's VM in the vmlab runtime directory"; return; }
    # Park the pointer away from the window, for a console that paints it into the frame.
    in_lab vmlab vm mouse-move "$VM" 1000 700 >/dev/null 2>&1
    python3 "$A/rfbgrab.py" "$sock" "$EV/$name.grab.json" --seconds $((TEAR_SECONDS + 300)) --stop "$STAGE/$name.stop" \
        --save-torn "$EV/$name-torn.png" >"$STAGE/$name.grab.out" 2>&1 &
    gpid=$!
    probe "$name" "$TEAR_SECONDS" tear_probe.exe --mode "$mode"
    local rc=$?
    : >"$STAGE/$name.stop"
    wait "$gpid"
    note "  $(tail -1 "$STAGE/$name.grab.out")"
    [ $rc = 0 ] && ran "$name" || skip "$name" "the tearing probe wrote no result"
}

check_tear_flip() { check_tear flip; }
check_tear_blt() { check_tear blt; }

ALL+=(tear-flip tear-blt)
