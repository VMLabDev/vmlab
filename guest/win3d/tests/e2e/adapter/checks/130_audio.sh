# Sourced by adapter.sh.
# check-shell-paths: no-errexit the adapter suite records failed checks and continues.

# ADAPTER_AUDIO_SECONDS  seconds the tone plays (60)
# ADAPTER_HOST_LOAD      busy host processes meanwhile (16; see 120_latency.sh)
# ADAPTER_AUDIO_TMP      directory the capture is written in (/dev/shm); it must be memory-backed

check_audio() {
    local lab node rec log offset ppid rpid i rc linked=0 sink= capdir wav
    local load=${ADAPTER_HOST_LOAD:-16} secs=${ADAPTER_AUDIO_SECONDS:-60}
    note "audio"
    command -v pw-record >/dev/null && command -v pw-link >/dev/null ||
        { skip audio "pw-record and pw-link are not installed on this host"; return; }
    lab=$(lab_name)
    [ -n "$lab" ] || { skip audio "found no lab daemon for this lab"; return; }
    # The host node of the VM's sound card. It exists while the guest plays.
    node="vmlab.$lab.$VM"
    rec="vmlab-adapter-audio-$$"
    # A stream plays only when it is linked to a sink. A PipeWire without
    # one (an account with no sound card and no pipewire-pulse) gets a null
    # sink for the length of the check.
    if ! pw-link -i 2>/dev/null | grep -q ':playback_FL$'; then
        sink="vmlab-adapter-sink-$$"
        pw-cli create-node adapter "{ factory.name=support.null-audio-sink node.name=$sink media.class=Audio/Sink object.linger=true audio.position=[FL FR] }" >/dev/null 2>&1
        note "  no sink in this PipeWire: made the null sink $sink"
    fi
    # The capture file is on tmpfs. pw-record writes it in the callback that
    # takes the sound server's buffers, and a write that waits for a disk
    # loses whole quanta: on btrfs once in three or four tones, about 30 s in
    # (the commit interval), as a break the VM did not make (card #912).
    capdir=$(mktemp -d "${ADAPTER_AUDIO_TMP:-/dev/shm}/vmlab-adapter-audio.XXXXXX" 2>/dev/null)
    if [ -z "$capdir" ] || [ ! -d "$capdir" ]; then
        capdir=$STAGE
        note "  ${ADAPTER_AUDIO_TMP:-/dev/shm} is not writable: the capture goes to $STAGE, where a disk write can cost frames"
    fi
    wav=$capdir/tone.wav
    log=$(hv_log)
    offset=$(stat -c %s "$log" 2>/dev/null || echo 0)
    # Record the VM's own node, linked by hand: the sink it plays into may
    # carry other sound. The recorder waits for the node, which appears when
    # the guest starts to play.
    pw-record --target 0 --rate 48000 --channels 2 --format s16 \
        -P "{ node.name = \"$rec\" node.autoconnect = false }" "$wav" >"$STAGE/tone.record.out" 2>&1 &
    rpid=$!
    host_load_start "$load"
    probe tone "$secs" tone_probe.exe &
    ppid=$!
    for i in $(seq 1 1200); do
        if pw-link -o 2>/dev/null | grep -qxF "$node:output_FL"; then
            pw-link "$node:output_FL" "$rec:input_FL" 2>>"$STAGE/tone.record.out" &&
                pw-link "$node:output_FR" "$rec:input_FR" 2>>"$STAGE/tone.record.out" && linked=1
            break
        fi
        kill -0 "$ppid" 2>/dev/null || break
        sleep 0.2
    done
    wait "$ppid"
    rc=$?
    sleep 1
    kill -INT "$rpid" 2>/dev/null
    wait "$rpid" 2>/dev/null
    host_load_stop
    [ -n "$sink" ] && pw-cli destroy "$sink" >/dev/null 2>&1
    printf '{"host_load": %s, "host_cpus": %s, "node": "%s", "linked": %s, "seconds": %s, "capture_fs": "%s"}\n' \
        "$load" "$(nproc)" "$node" "$linked" "$secs" "$(stat -f -c %T "$capdir" 2>/dev/null)" >"$EV/tone.host.json"
    python3 "$A/audio.py" "$wav" "$EV/tone.capture.json" --hv-log "$log" --hv-log-offset "$offset" \
        >"$STAGE/tone.capture.out" 2>&1
    # The capture stays in the stage directory for a second look, as before.
    if [ "$capdir" != "$STAGE" ] && [ -n "$STAGE" ] && [ -d "$STAGE" ]; then
        [ -f "$wav" ] && mv -f "$wav" "$STAGE/tone.wav"
        rmdir "$capdir" 2>/dev/null
    fi
    note "  $(tail -1 "$STAGE/tone.capture.out")"
    if [ $rc != 0 ]; then
        skip audio "the tone probe wrote no result"
    elif [ $linked = 0 ]; then
        skip audio "the host node $node did not appear, or the capture did not link to it"
    else
        ran audio
    fi
}

ALL+=(audio)
