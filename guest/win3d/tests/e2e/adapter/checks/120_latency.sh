# Sourced by adapter.sh.
# check-shell-paths: no-errexit the adapter suite records failed checks and continues.

# ADAPTER_HOST_LOAD     busy host processes while `latency` and `audio` measure.
#                       Not set: none for `latency`, 16 for `audio`.
# ADAPTER_LATENCY_MOVES pointer moves the latency check times (120)

LOAD_PIDS=()
host_load_start() { # host_load_start N: N processes that spin, for at most 15 minutes
    local i
    LOAD_PIDS=()
    for i in $(seq 1 "$1"); do
        timeout 900 sh -c 'while :; do :; done' &
        LOAD_PIDS+=($!)
    done
    [ "$1" -gt 0 ] && note "  host load: $1 busy processes"
    return 0
}
host_load_stop() {
    [ ${#LOAD_PIDS[@]} -gt 0 ] && kill "${LOAD_PIDS[@]}" 2>/dev/null
    wait "${LOAD_PIDS[@]}" 2>/dev/null
    LOAD_PIDS=()
}

check_latency() {
    local sock gpid rc load=${ADAPTER_HOST_LOAD:-0} moves=${ADAPTER_LATENCY_MOVES:-120}
    note "latency"
    sock=$(vnc_sock)
    [ -n "$sock" ] || { skip latency "found no vnc.sock for this lab's VM in the vmlab runtime directory"; return; }
    host_load_start "$load"
    python3 "$A/latency.py" "$sock" "$EV/latency.grab.json" --moves "$moves" --wait 900 --stop "$STAGE/latency.stop" \
        >"$STAGE/latency.grab.out" 2>&1 &
    gpid=$!
    # A move takes a quarter of a second and its answer; the window must outlast them.
    probe latency $((moves / 2 + 30)) latency_probe.exe
    rc=$?
    : >"$STAGE/latency.stop"
    wait "$gpid"
    host_load_stop
    printf '{"host_load": %s, "host_cpus": %s}\n' "$load" "$(nproc)" >"$EV/latency.host.json"
    note "  $(tail -1 "$STAGE/latency.grab.out")"
    [ $rc = 0 ] && ran latency || skip latency "the latency probe wrote no result"
}

ALL+=(latency)
